//! The `archive` tool (port of `OpenOrca.Tools.Archive`): create, extract and list zip files.
//!
//! Zips are written with deflate (`CompressionLevel.Optimal` in C#); the compressed bytes and
//! so the reported archive size differ from .NET's. Entry times are stored as UTC (.NET stores
//! local time; the port has no time-zone database).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use orca_abstractions::{Tool, ToolContext, ToolRenderHints, ToolResult, ToolRiskLevel};
use orca_tools_common::{
    append_line, group_thousands, io_message, is_dangerous_path, required_str,
};
use serde_json::Value;

/// Every tool in this crate.
pub fn all_tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(ArchiveTool)]
}

fn schema(text: &str) -> Value {
    serde_json::from_str(text).expect("tool schema is valid JSON")
}

fn display(p: &Path) -> String {
    p.display().to_string()
}

pub struct ArchiveTool;

#[async_trait::async_trait]
impl Tool for ArchiveTool {
    fn name(&self) -> &str {
        "archive"
    }

    fn description(&self) -> &str {
        "Create, extract, or list contents of zip archives. Use action 'create' to zip files/directories, 'extract' to unzip, 'list' to view contents."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f4e5}",
            short_description: "Archive operation",
            summary_arg_keys: &[],
            aliases: &["extract"],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["create", "extract", "list"],
                "description": "Action: 'create' a zip archive, 'extract' a zip archive, or 'list' its contents"
            },
            "archive_path": {
                "type": "string",
                "description": "Path to the zip archive file"
            },
            "source_path": {
                "type": "string",
                "description": "Path to compress (file or directory). Required for 'create'."
            },
            "output_path": {
                "type": "string",
                "description": "Directory to extract to. Required for 'extract'."
            }
        },
        "required": ["action", "archive_path"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let (action, archive) = match (
            required_str(args, "action"),
            required_str(args, "archive_path"),
        ) {
            (Ok(a), Ok(p)) => (a.to_string(), ctx.full_path(p)),
            (Err(e), _) | (_, Err(e)) => return ToolResult::error(e),
        };
        let args = args.clone();
        let ctx = ctx.clone();
        // Zip work is blocking I/O.
        tokio::task::spawn_blocking(move || match action.as_str() {
            "create" => create(&archive, &args, &ctx),
            "extract" => extract(&archive, &args, &ctx),
            "list" => list(&archive),
            other => ToolResult::error(format!(
                "Unknown action: {other}. Use 'create', 'extract', or 'list'."
            )),
        })
        .await
        .unwrap_or_else(|e| ToolResult::error(format!("Error: {e}")))
    }
}

fn create(archive: &Path, args: &Value, ctx: &ToolContext) -> ToolResult {
    let Some(source) = args.get("source_path").and_then(Value::as_str) else {
        return ToolResult::error("'source_path' is required for action 'create'.");
    };
    let source = ctx.full_path(source);
    if is_dangerous_path(&source) {
        return ToolResult::error(format!(
            "Refusing to archive dangerous path: {}",
            display(&source)
        ));
    }
    if !source.exists() {
        return ToolResult::error(format!("Source not found: {}", display(&source)));
    }
    match write_zip(archive, &source) {
        Ok(len) => ToolResult::success(format!(
            "Created: {} ({} bytes)",
            display(archive),
            group_thousands(len as usize)
        )),
        Err(e) => ToolResult::error(format!("Error creating archive: {e}")),
    }
}

fn zip_err(e: zip::result::ZipError) -> String {
    match e {
        zip::result::ZipError::Io(io) => io_message(&io),
        other => other.to_string(),
    }
}

/// The file's modification time as a zip (DOS) time.
fn zip_time(path: &Path) -> zip::DateTime {
    let secs = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    zip::DateTime::from_date_and_time(
        y.clamp(1980, 2107) as u16,
        m as u8,
        d as u8,
        (tod / 3600) as u8,
        ((tod / 60) % 60) as u8,
        (tod % 60) as u8,
    )
    .unwrap_or_default()
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `ZipFile.CreateFromDirectory(..., includeBaseDirectory: true)` for a directory, or one
/// entry named after the file. Returns the archive size.
fn write_zip(archive: &Path, source: &Path) -> Result<u64, String> {
    if let Some(dir) = archive.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io_message(&e))?;
    }
    if archive.is_file() {
        std::fs::remove_file(archive).map_err(|e| io_message(&e))?;
    }
    let file = std::fs::File::create(archive).map_err(|e| io_message(&e))?;
    let mut zip = zip::ZipWriter::new(file);
    let options = |p: &Path| {
        zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .last_modified_time(zip_time(p))
    };
    let base_name = source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    if source.is_dir() {
        for entry in walkdir::WalkDir::new(source).min_depth(1) {
            let entry = entry.map_err(|e| e.to_string())?;
            let rel = entry
                .path()
                .strip_prefix(source)
                .unwrap_or(entry.path())
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let name = format!("{base_name}/{rel}");
            if entry.file_type().is_dir() {
                // Only empty directories get an entry of their own (as in .NET).
                let empty = std::fs::read_dir(entry.path())
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(false);
                if empty {
                    zip.add_directory(format!("{name}/"), options(entry.path()))
                        .map_err(zip_err)?;
                }
            } else {
                zip.start_file(name, options(entry.path()))
                    .map_err(zip_err)?;
                copy_into(entry.path(), &mut zip)?;
            }
        }
        // An empty source directory still records the base directory.
        if std::fs::read_dir(source)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false)
        {
            zip.add_directory(format!("{base_name}/"), options(source))
                .map_err(zip_err)?;
        }
    } else {
        zip.start_file(base_name, options(source))
            .map_err(zip_err)?;
        copy_into(source, &mut zip)?;
    }
    zip.finish().map_err(zip_err)?;
    std::fs::metadata(archive)
        .map(|m| m.len())
        .map_err(|e| io_message(&e))
}

fn copy_into(path: &Path, out: &mut impl Write) -> Result<(), String> {
    let mut f = std::fs::File::open(path).map_err(|e| io_message(&e))?;
    std::io::copy(&mut f, out)
        .map(|_| ())
        .map_err(|e| io_message(&e))
}

fn open_archive(archive: &Path) -> Result<zip::ZipArchive<std::fs::File>, String> {
    let f = std::fs::File::open(archive).map_err(|e| io_message(&e))?;
    zip::ZipArchive::new(f).map_err(zip_err)
}

fn extract(archive: &Path, args: &Value, ctx: &ToolContext) -> ToolResult {
    if !archive.is_file() {
        return ToolResult::error(format!("Archive not found: {}", display(archive)));
    }
    let output: PathBuf = match args.get("output_path").and_then(Value::as_str) {
        Some(p) => ctx.full_path(p),
        None => archive.parent().map(Path::to_path_buf).unwrap_or_default(),
    };
    if is_dangerous_path(&output) {
        return ToolResult::error(format!(
            "Refusing to extract to dangerous path: {}",
            display(&output)
        ));
    }
    match extract_all(archive, &output) {
        Ok(Ok(count)) => ToolResult::success(format!(
            "Extracted {count} entries to: {}",
            display(&output)
        )),
        Ok(Err(bad_entry)) => ToolResult::error(format!(
            "Archive contains path traversal entry: {bad_entry}"
        )),
        Err(e) => ToolResult::error(format!("Error extracting archive: {e}")),
    }
}

/// Ok(Ok(entry count)), Ok(Err(entry)) for a zip-slip entry (nothing extracted), or an error.
fn extract_all(archive: &Path, output: &Path) -> Result<Result<usize, String>, String> {
    std::fs::create_dir_all(output).map_err(|e| io_message(&e))?;
    let mut zip = open_archive(archive)?;
    let root = output.to_string_lossy().to_lowercase();
    let mut targets = Vec::with_capacity(zip.len());
    for i in 0..zip.len() {
        let entry = zip.by_index_raw(i).map_err(zip_err)?;
        let name = entry.name().map_err(zip_err)?.into_owned();
        let dest = orca_abstractions::full_path(output, &name);
        if !dest.to_string_lossy().to_lowercase().starts_with(&root) {
            return Ok(Err(name));
        }
        targets.push((dest, name.ends_with('/') || name.ends_with('\\')));
    }
    for (i, (dest, is_dir)) in targets.iter().enumerate() {
        if *is_dir {
            std::fs::create_dir_all(dest).map_err(|e| io_message(&e))?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_message(&e))?;
        }
        let mut entry = zip.by_index(i).map_err(zip_err)?;
        let mut data = Vec::new();
        entry.read_to_end(&mut data).map_err(|e| io_message(&e))?;
        std::fs::write(dest, data).map_err(|e| io_message(&e))?;
    }
    Ok(Ok(targets.len()))
}

fn list(archive: &Path) -> ToolResult {
    if !archive.is_file() {
        return ToolResult::error(format!("Archive not found: {}", display(archive)));
    }
    match list_entries(archive) {
        Ok(text) => ToolResult::success(text),
        Err(e) => ToolResult::error(format!("Error reading archive: {e}")),
    }
}

fn list_entries(archive: &Path) -> Result<String, String> {
    let mut zip = open_archive(archive)?;
    let mut sb = String::new();
    append_line(&mut sb, &format!("Archive: {}", display(archive)));
    append_line(&mut sb, &format!("Entries: {}", zip.len()));
    append_line(&mut sb, "");
    let mut total: u64 = 0;
    let count = zip.len();
    for i in 0..count {
        let entry = zip.by_index_raw(i).map_err(zip_err)?;
        total += entry.size();
        if i < 100 {
            let name = entry.name().map_err(zip_err)?.into_owned();
            let t = entry.last_modified().unwrap_or_default();
            append_line(
                &mut sb,
                &format!(
                    "  {:<50} {:>12} bytes  {:04}-{:02}-{:02} {:02}:{:02}",
                    name,
                    group_thousands(entry.size() as usize),
                    t.year(),
                    t.month(),
                    t.day(),
                    t.hour(),
                    t.minute()
                ),
            );
        }
    }
    if count > 100 {
        append_line(&mut sb, &format!("  ... ({} more entries)", count - 100));
    }
    append_line(&mut sb, "");
    append_line(
        &mut sb,
        &format!(
            "Total uncompressed: {} bytes",
            group_thousands(total as usize)
        ),
    );
    Ok(sb)
}
