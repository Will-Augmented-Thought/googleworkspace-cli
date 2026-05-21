// Copyright 2026 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Gmail `+download-attachments` helper.

use super::*;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug)]
struct DownloadAttachmentsConfig {
    message_id: String,
    output_dir: PathBuf,
    include_inline: bool,
    overwrite: bool,
    dry_run: bool,
}

#[derive(Debug)]
struct PlannedAttachment<'a> {
    original_index: usize,
    part: &'a OriginalPart,
    filename: String,
    path: PathBuf,
}

#[derive(Debug, Serialize)]
struct DownloadedAttachment {
    part_index: usize,
    filename: String,
    path: String,
    mime_type: String,
    size_bytes: u64,
    attachment_id: String,
    inline: bool,
    content_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct DownloadAttachmentsResult {
    message_id: String,
    output_dir: String,
    dry_run: bool,
    downloaded_count: usize,
    skipped_inline_count: usize,
    attachments: Vec<DownloadedAttachment>,
}

pub(super) fn command() -> Command {
    Command::new("+download-attachments")
        .alias("+attachments")
        .about("[Helper] Download attachments from a Gmail message")
        .arg(
            Arg::new("id")
                .long("id")
                .alias("message-id")
                .required(true)
                .help("The Gmail message ID to download attachments from")
                .value_name("ID"),
        )
        .arg(
            Arg::new("output-dir")
                .long("output-dir")
                .required(true)
                .help("Relative directory under the current workspace where files are saved")
                .value_name("DIR"),
        )
        .arg(
            Arg::new("include-inline")
                .long("include-inline")
                .help("Also download inline MIME parts, such as embedded images")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("overwrite")
                .long("overwrite")
                .help("Overwrite existing files with the same sanitized attachment filename")
                .action(ArgAction::SetTrue),
        )
        .after_help(
            "\
EXAMPLES:
  gws gmail +download-attachments --id 18f1a2b3c4d --output-dir ./downloads/gmail
  gws gmail +download-attachments --message-id 18f1a2b3c4d --output-dir ./downloads/gmail --include-inline

TIPS:
  Writes only under a validated relative --output-dir.
  Regular file attachments are downloaded by default; inline images are skipped unless --include-inline is set.
  Existing files are not overwritten unless --overwrite is set.
  Output is JSON with exact saved paths and attachment metadata.",
        )
}

/// Handle the `+download-attachments` subcommand.
pub(super) async fn handle_download_attachments(matches: &ArgMatches) -> Result<(), GwsError> {
    let config = parse_download_config(matches)?;

    if config.dry_run {
        let result = DownloadAttachmentsResult {
            message_id: config.message_id,
            output_dir: display_path(config.output_dir.as_path()),
            dry_run: true,
            downloaded_count: 0,
            skipped_inline_count: 0,
            attachments: Vec::new(),
        };
        print_download_result(&result)?;
        return Ok(());
    }

    let token = auth::get_token(&[GMAIL_READONLY_SCOPE])
        .await
        .map_err(|e| GwsError::Auth(format!("Gmail auth failed: {e}")))?;
    let client = crate::client::build_client()?;

    let original = fetch_message_metadata(&client, &token, &config.message_id).await?;
    let skipped_inline_count = if config.include_inline {
        0
    } else {
        original
            .parts
            .iter()
            .filter(|part| part.is_inline())
            .count()
    };
    let selected_parts = select_download_parts(&original.parts, config.include_inline);
    let planned = plan_downloads(
        selected_parts.as_slice(),
        config.output_dir.as_path(),
        config.overwrite,
    )?;

    if planned.is_empty() {
        let result = DownloadAttachmentsResult {
            message_id: config.message_id,
            output_dir: display_path(config.output_dir.as_path()),
            dry_run: false,
            downloaded_count: 0,
            skipped_inline_count,
            attachments: Vec::new(),
        };
        print_download_result(&result)?;
        return Ok(());
    }

    std::fs::create_dir_all(&config.output_dir).map_err(|e| {
        GwsError::Validation(format!(
            "Failed to create --output-dir '{}': {e}",
            config.output_dir.display()
        ))
    })?;

    let mut downloaded = Vec::with_capacity(planned.len());
    for item in planned {
        let data = fetch_attachment_data(
            &client,
            &token,
            &config.message_id,
            &item.part.attachment_id,
        )
        .await?;
        write_attachment_file(item.path.as_path(), &data, config.overwrite)?;
        downloaded.push(DownloadedAttachment {
            part_index: item.original_index,
            filename: item.filename,
            path: display_path(item.path.as_path()),
            mime_type: item.part.content_type.clone(),
            size_bytes: data.len() as u64,
            attachment_id: item.part.attachment_id.clone(),
            inline: item.part.is_inline(),
            content_id: item.part.content_id.clone(),
        });
    }

    let result = DownloadAttachmentsResult {
        message_id: config.message_id,
        output_dir: display_path(config.output_dir.as_path()),
        dry_run: false,
        downloaded_count: downloaded.len(),
        skipped_inline_count,
        attachments: downloaded,
    };
    print_download_result(&result)?;
    Ok(())
}

fn parse_download_config(matches: &ArgMatches) -> Result<DownloadAttachmentsConfig, GwsError> {
    let message_id = matches.get_one::<String>("id").ok_or_else(|| {
        GwsError::Validation("--id is required for +download-attachments".to_string())
    })?;
    crate::validate::reject_dangerous_chars(message_id, "--id")?;

    let output_dir_raw = matches.get_one::<String>("output-dir").ok_or_else(|| {
        GwsError::Validation("--output-dir is required for +download-attachments".to_string())
    })?;
    let output_dir = crate::validate::validate_safe_output_dir(output_dir_raw)?;

    Ok(DownloadAttachmentsConfig {
        message_id: message_id.to_string(),
        output_dir,
        include_inline: matches.get_flag("include-inline"),
        overwrite: matches.get_flag("overwrite"),
        dry_run: matches.get_flag("dry-run"),
    })
}

fn select_download_parts(
    parts: &[OriginalPart],
    include_inline: bool,
) -> Vec<(usize, &OriginalPart)> {
    parts
        .iter()
        .enumerate()
        .filter(|(_, part)| include_inline || !part.is_inline())
        .collect()
}

fn plan_downloads<'a>(
    parts: &[(usize, &'a OriginalPart)],
    output_dir: &Path,
    overwrite: bool,
) -> Result<Vec<PlannedAttachment<'a>>, GwsError> {
    let mut used_names = HashSet::new();
    let mut planned = Vec::with_capacity(parts.len());

    for (original_index, part) in parts {
        let base_name =
            sanitize_download_filename(&part.filename, *original_index, &part.content_type);
        let filename = unique_filename(&base_name, &mut used_names);
        let path = output_dir.join(&filename);
        ensure_target_path(output_dir, &path, overwrite)?;
        planned.push(PlannedAttachment {
            original_index: *original_index,
            part,
            filename,
            path,
        });
    }

    Ok(planned)
}

fn sanitize_download_filename(raw: &str, part_index: usize, mime_type: &str) -> String {
    let fallback = synthesize_filename(part_index, mime_type);
    let source = if raw.trim().is_empty() {
        fallback.as_str()
    } else {
        raw
    };

    let mut cleaned = String::with_capacity(source.len());
    for ch in source.chars() {
        if is_invalid_filename_char(ch) {
            cleaned.push('_');
        } else {
            cleaned.push(ch);
        }
    }

    let trimmed = cleaned.trim_matches([' ', '.']).trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        fallback
    } else {
        trimmed.to_string()
    }
}

fn is_invalid_filename_char(ch: char) -> bool {
    ch.is_control()
        || crate::validate::is_dangerous_unicode(ch)
        || matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
}

fn unique_filename(base_name: &str, used_names: &mut HashSet<String>) -> String {
    if used_names.insert(base_name.to_string()) {
        return base_name.to_string();
    }

    let mut suffix = 2;
    loop {
        let candidate = append_filename_suffix(base_name, suffix);
        if used_names.insert(candidate.clone()) {
            return candidate;
        }
        suffix += 1;
    }
}

fn append_filename_suffix(base_name: &str, suffix: usize) -> String {
    if let Some((stem, extension)) = base_name.rsplit_once('.') {
        if !stem.is_empty() && !extension.is_empty() {
            return format!("{stem}-{suffix}.{extension}");
        }
    }
    format!("{base_name}-{suffix}")
}

fn ensure_target_path(output_dir: &Path, path: &Path, overwrite: bool) -> Result<(), GwsError> {
    if !path.starts_with(output_dir) {
        return Err(GwsError::Validation(format!(
            "Attachment target '{}' escapes --output-dir '{}'",
            path.display(),
            output_dir.display()
        )));
    }

    if path.exists() && !overwrite {
        return Err(GwsError::Validation(format!(
            "Refusing to overwrite existing file '{}'; pass --overwrite to replace it",
            path.display()
        )));
    }

    Ok(())
}

fn write_attachment_file(path: &Path, data: &[u8], overwrite: bool) -> Result<(), GwsError> {
    let mut options = OpenOptions::new();
    options.write(true);
    if overwrite {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }

    let mut file = options.open(path).map_err(|e| {
        GwsError::Validation(format!(
            "Failed to create attachment file '{}': {e}",
            path.display()
        ))
    })?;
    file.write_all(data).map_err(|e| {
        GwsError::Validation(format!(
            "Failed to write attachment file '{}': {e}",
            path.display()
        ))
    })?;
    file.sync_all().map_err(|e| {
        GwsError::Validation(format!(
            "Failed to sync attachment file '{}': {e}",
            path.display()
        ))
    })?;
    Ok(())
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}

fn print_download_result(result: &DownloadAttachmentsResult) -> Result<(), GwsError> {
    let output = serde_json::to_string_pretty(result)
        .context("Failed to serialize attachment download result")?;
    println!("{output}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn regular_part(filename: &str, attachment_id: &str) -> OriginalPart {
        OriginalPart {
            filename: filename.to_string(),
            content_type: "application/pdf".to_string(),
            size: 12,
            attachment_id: attachment_id.to_string(),
            content_id: None,
        }
    }

    fn inline_part(filename: &str, attachment_id: &str) -> OriginalPart {
        OriginalPart {
            filename: filename.to_string(),
            content_type: "image/png".to_string(),
            size: 12,
            attachment_id: attachment_id.to_string(),
            content_id: Some("image@example.com".to_string()),
        }
    }

    #[test]
    fn command_requires_id_and_output_dir() {
        let err = command()
            .try_get_matches_from(["+download-attachments", "--id", "msg-1"])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn command_accepts_alias() {
        let cmd = Command::new("gmail").subcommand(command());
        let matches = cmd
            .try_get_matches_from([
                "gmail",
                "+attachments",
                "--id",
                "msg-1",
                "--output-dir",
                "downloads",
            ])
            .unwrap();
        assert!(matches
            .subcommand_matches("+download-attachments")
            .is_some());
    }

    #[tokio::test]
    async fn handle_dry_run_returns_before_auth() {
        let cmd = Command::new("gws")
            .arg(
                Arg::new("dry-run")
                    .long("dry-run")
                    .action(ArgAction::SetTrue)
                    .global(true),
            )
            .subcommand(Command::new("gmail").subcommand(command()));
        let matches = cmd
            .try_get_matches_from([
                "gws",
                "gmail",
                "+download-attachments",
                "--id",
                "msg-1",
                "--output-dir",
                "downloads",
                "--dry-run",
            ])
            .unwrap();
        let gmail_matches = matches.subcommand_matches("gmail").unwrap();
        let download_matches = gmail_matches
            .subcommand_matches("+download-attachments")
            .unwrap();

        handle_download_attachments(download_matches).await.unwrap();
    }

    #[test]
    fn select_download_parts_skips_inline_by_default() {
        let parts = vec![
            regular_part("report.pdf", "att-1"),
            inline_part("logo.png", "att-2"),
        ];
        let selected = select_download_parts(&parts, false);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].1.attachment_id, "att-1");
    }

    #[test]
    fn select_download_parts_includes_inline_when_requested() {
        let parts = vec![
            regular_part("report.pdf", "att-1"),
            inline_part("logo.png", "att-2"),
        ];
        let selected = select_download_parts(&parts, true);
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn sanitize_download_filename_replaces_path_and_control_chars() {
        let filename = sanitize_download_filename("../bad\u{202e}\nname.pdf", 3, "application/pdf");
        assert_eq!(filename, "_bad__name.pdf");
    }

    #[test]
    fn sanitize_download_filename_uses_synthesized_name_when_empty() {
        let filename = sanitize_download_filename("\n\t", 2, "image/jpeg");
        assert_eq!(filename, "part-2.jpg");
    }

    #[test]
    fn unique_filename_suffixes_duplicate_names() {
        let mut used = HashSet::new();
        assert_eq!(unique_filename("report.pdf", &mut used), "report.pdf");
        assert_eq!(unique_filename("report.pdf", &mut used), "report-2.pdf");
        assert_eq!(unique_filename("archive", &mut used), "archive");
        assert_eq!(unique_filename("archive", &mut used), "archive-2");
    }

    #[test]
    fn plan_downloads_rejects_existing_file_without_overwrite() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("report.pdf");
        std::fs::write(&target, b"existing").unwrap();
        let parts = vec![regular_part("report.pdf", "att-1")];
        let selected = select_download_parts(&parts, false);
        let err = plan_downloads(&selected, temp.path(), false).unwrap_err();
        assert!(err.to_string().contains("Refusing to overwrite"));
    }

    #[test]
    fn plan_downloads_allows_existing_file_with_overwrite() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("report.pdf");
        std::fs::write(&target, b"existing").unwrap();
        let parts = vec![regular_part("report.pdf", "att-1")];
        let selected = select_download_parts(&parts, false);
        let planned = plan_downloads(&selected, temp.path(), true).unwrap();
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].path, target);
    }

    #[test]
    fn write_attachment_file_rejects_existing_without_overwrite() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("report.pdf");
        std::fs::write(&target, b"existing").unwrap();
        let err = write_attachment_file(&target, b"new", false).unwrap_err();
        assert!(err.to_string().contains("Failed to create attachment file"));
        assert_eq!(std::fs::read(&target).unwrap(), b"existing");
    }

    #[test]
    fn write_attachment_file_overwrites_when_requested() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("report.pdf");
        std::fs::write(&target, b"existing").unwrap();
        write_attachment_file(&target, b"new", true).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
    }
}
