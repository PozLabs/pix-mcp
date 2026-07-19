//! Analysis tool logic: parse captures, counters, comparisons, and frame
//! insights via `pixtool.exe`.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use csv::{ReaderBuilder, StringRecord, WriterBuilder};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tempfile::{Builder as TempFileBuilder, TempDir, TempPath};
use tokio::process::Command;

use crate::pix::PixTool;
#[cfg(test)]
use crate::pix::pixtool::PROCESS_OUTPUT_TRUNCATION_MARKER;
use crate::pix::pixtool::{
    PROCESS_OUTPUT_DIAGNOSTIC_PREFIX, check_developer_mode, push_value_option, run_pixtool_command,
};
use crate::security;

/// How much detail to return inline from list-style tools.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ResponseFormat {
    /// Compact: smaller default page size, counts + a preview slice.
    Summary,
    /// Detailed: larger default page size (still capped).
    Full,
}

impl ResponseFormat {
    fn default_limit(self) -> usize {
        match self {
            ResponseFormat::Summary => 50,
            ResponseFormat::Full => 500,
        }
    }
}

const EVENT_LIST_MAX_LIMIT: usize = 2000;
const MAX_INLINE_EVENT_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_INLINE_ANALYSIS_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_ANALYZE_EVENTS_JSON_BYTES: usize = 512 * 1024;
const MAX_ANALYZE_PREVIEW_ROW_BYTES: usize = 128 * 1024;
const MAX_INLINE_PROCESS_OUTPUT_BYTES: usize = 32 * 1024;
const MAX_FRAME_TEXT_FIELD_BYTES: usize = 64 * 1024;
const MAX_FRAME_COLUMNS_JSON_BYTES: usize = 192 * 1024;
const COUNTERS_MAX_LIMIT: usize = 2000;
const MAX_INLINE_COUNTERS_JSON_BYTES: usize = 768 * 1024;
const MAX_COUNTER_NAME_BYTES: usize = 16 * 1024;
const MAX_INLINE_DIAGNOSTICS_JSON_BYTES: usize = 192 * 1024;
const SCREENSHOT_MAX_DIMENSION: u32 = 4096;
const ANALYSIS_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_ANALYSIS_CSV_BYTES: u64 = 128 * 1024 * 1024;
const MAX_ANALYSIS_CSV_ROWS: usize = 250_000;
const MAX_COUNTER_EXPORT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_COUNTER_EXPORT_ROWS: usize = 50_000;
const MAX_COUNTER_RESULT_BYTES: usize = 8 * 1024 * 1024;
const MAX_SCREENSHOT_SOURCE_DIMENSION: u32 = 16_384;
const MAX_SCREENSHOT_DECODE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_EMBEDDED_SCREENSHOT_BYTES: usize = 8 * 1024 * 1024;
const COMPARE_DEFAULT_MAX_EVENTS: usize = 20_000;
const COMPARE_MAX_EVENTS: usize = 50_000;
const COMPARE_DEFAULT_MAX_CHANGES: usize = 20;
const COMPARE_MAX_CHANGES: usize = 50;
const MAX_COMPARE_COLUMNS: usize = 4_096;
const MAX_COMPARE_HEADER_BYTES: usize = 512 * 1024;
const MAX_COMPARE_EVENT_FIELD_BYTES: usize = 4 * 1024;
const MAX_COMPARE_WARNING_BYTES: usize = 8 * 1024;
const MAX_COMPARE_REPORT_BYTES: usize = 1024 * 1024;

fn require_capture_file(raw_path: &str) -> Result<PathBuf> {
    if raw_path.trim().is_empty() {
        return Err(anyhow::anyhow!("capture_path must not be empty"));
    }
    let requested_path = security::resolve_artifact_path(Path::new(raw_path), "Capture path")?;
    if !requested_path.is_file() {
        return Err(capture_not_found(raw_path));
    }
    let path = security::validate_input_file(&requested_path, "Capture path")?;
    if std::fs::metadata(&path)?.len() == 0 {
        return Err(anyhow::anyhow!("Capture file is empty: {}", raw_path));
    }
    if !path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wpix"))
    {
        return Err(anyhow::anyhow!(
            "Capture path must reference a .wpix file: {}",
            raw_path
        ));
    }
    Ok(path)
}

async fn require_capture_file_async(raw_path: String) -> Result<PathBuf> {
    tokio::task::spawn_blocking(move || require_capture_file(&raw_path))
        .await
        .context("Capture validation task failed")?
}

fn event_list_limit(requested: Option<usize>, format: ResponseFormat) -> Result<usize> {
    let limit = requested.unwrap_or_else(|| format.default_limit());
    if limit == 0 {
        return Err(anyhow::anyhow!(
            "limit must be at least 1 (maximum {})",
            EVENT_LIST_MAX_LIMIT
        ));
    }
    Ok(limit.min(EVENT_LIST_MAX_LIMIT))
}

fn event_output_path(raw: Option<&str>) -> Result<Option<PathBuf>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if raw.trim().is_empty() {
        return Err(anyhow::anyhow!("output_path must not be empty"));
    }
    let path = PathBuf::from(raw);
    if !path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("csv"))
    {
        return Err(anyhow::anyhow!(
            "output_path must end with .csv; refusing to risk overwriting a non-CSV file"
        ));
    }
    Ok(Some(security::validate_output_file(
        &path,
        "Event-list output path",
    )?))
}

/// Parsed UTF-8 CSV data. `csv::StringRecord` preserves logical records even
/// when quoted fields contain commas, escaped quotes, or embedded newlines.
#[derive(Debug)]
struct ParsedCsv {
    headers: StringRecord,
    records: Vec<StringRecord>,
}

fn parse_csv_bytes(bytes: &[u8]) -> Result<ParsedCsv> {
    parse_csv_bytes_with_limit(bytes, MAX_ANALYSIS_CSV_ROWS)
}

fn parse_csv_bytes_with_limit(bytes: &[u8], max_rows: usize) -> Result<ParsedCsv> {
    // PIX exports may start with an UTF-8 BOM. Strip it explicitly so the first
    // column can be matched by name and is not exposed to callers with a hidden
    // U+FEFF prefix.
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    if bytes.is_empty() {
        return Err(anyhow::anyhow!("Empty CSV file"));
    }

    let mut reader = ReaderBuilder::new().flexible(false).from_reader(bytes);
    let headers = reader.headers().context("Invalid CSV header")?.clone();
    if headers.is_empty() {
        return Err(anyhow::anyhow!("CSV file has no header columns"));
    }
    if headers.iter().any(|header| header.trim().is_empty()) {
        return Err(anyhow::anyhow!("CSV file contains an empty header column"));
    }

    let mut records = Vec::new();
    for (index, record) in reader.records().enumerate() {
        if index >= max_rows {
            return Err(anyhow::anyhow!(
                "CSV contains more than the supported maximum of {} data rows",
                max_rows
            ));
        }
        records
            .push(record.with_context(|| format!("Invalid CSV record at data row {}", index + 1))?);
    }

    Ok(ParsedCsv { headers, records })
}

fn read_file_limited(path: &Path, max_bytes: u64, label: &str) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("{label} not found: {}", path.display()))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(anyhow::anyhow!("{label} is not a file: {}", path.display()));
    }
    if metadata.len() == 0 {
        return Err(anyhow::anyhow!("{label} is empty: {}", path.display()));
    }
    if metadata.len() > max_bytes {
        return Err(anyhow::anyhow!(
            "{label} is too large ({} bytes; maximum {}): {}",
            metadata.len(),
            max_bytes,
            path.display()
        ));
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(max_bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(anyhow::anyhow!(
            "{label} grew beyond the maximum of {} bytes while it was read: {}",
            max_bytes,
            path.display()
        ));
    }
    Ok(bytes)
}

fn read_csv_file(path: &Path) -> Result<ParsedCsv> {
    let bytes = read_file_limited(path, MAX_ANALYSIS_CSV_BYTES, "CSV output")?;
    let parsed = parse_csv_bytes(&bytes)?;
    validate_event_list_headers(&parsed.headers)?;
    if parsed.records.is_empty() {
        return Err(anyhow::anyhow!(
            "CSV output contains a header but no data rows: {}",
            path.display()
        ));
    }
    Ok(parsed)
}

fn validate_event_list_headers(headers: &StringRecord) -> Result<()> {
    let normalized_headers: std::collections::HashSet<String> = headers
        .iter()
        .map(|header| header.trim().to_ascii_lowercase())
        .collect();
    for required in ["queue id", "name", "global id"] {
        if !normalized_headers.contains(required) {
            return Err(anyhow::anyhow!(
                "CSV output is not a PIX event list: required column {:?} is missing",
                required
            ));
        }
    }
    Ok(())
}

#[derive(Debug)]
struct EventCsvSummary {
    header: String,
    total_events: usize,
}

/// Validate and count a file-backed event list without retaining all rows.
/// This is used for full on-disk exports so they do not consume equivalent heap
/// memory or hit the inline response row cap.
fn inspect_event_csv_file(path: &Path) -> Result<EventCsvSummary> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("CSV output not found: {}", path.display()))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(anyhow::anyhow!(
            "CSV output is missing or empty: {}",
            path.display()
        ));
    }
    if metadata.len() > MAX_ANALYSIS_CSV_BYTES {
        return Err(anyhow::anyhow!(
            "CSV output is too large ({} bytes; maximum {}): {}",
            metadata.len(),
            MAX_ANALYSIS_CSV_BYTES,
            path.display()
        ));
    }

    let mut prefix = [0_u8; 3];
    let prefix_len = file.read(&mut prefix)?;
    let start = if prefix_len == prefix.len() && prefix == [0xEF, 0xBB, 0xBF] {
        3
    } else {
        0
    };
    file.seek(SeekFrom::Start(start))?;

    let remaining_limit = MAX_ANALYSIS_CSV_BYTES.saturating_sub(start);
    let limited_file = file.take(remaining_limit.saturating_add(1));
    let mut reader = ReaderBuilder::new()
        .flexible(false)
        .from_reader(limited_file);
    let headers = reader.headers().context("Invalid CSV header")?.clone();
    if headers.is_empty() || headers.iter().any(|header| header.trim().is_empty()) {
        return Err(anyhow::anyhow!(
            "CSV file must contain non-empty header columns"
        ));
    }
    validate_event_list_headers(&headers)?;

    let mut total_events = 0usize;
    for record in reader.records() {
        record.with_context(|| format!("Invalid CSV record at data row {}", total_events + 1))?;
        total_events = total_events
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("CSV row count overflow"))?;
    }
    if reader.into_inner().limit() == 0 {
        return Err(anyhow::anyhow!(
            "CSV output grew beyond the maximum of {} bytes while it was validated: {}",
            MAX_ANALYSIS_CSV_BYTES,
            path.display()
        ));
    }
    if total_events == 0 {
        return Err(anyhow::anyhow!(
            "CSV output contains a header but no data rows: {}",
            path.display()
        ));
    }

    Ok(EventCsvSummary {
        header: csv_record_to_string(&headers)?,
        total_events,
    })
}

async fn read_csv_file_async(path: PathBuf) -> Result<ParsedCsv> {
    tokio::task::spawn_blocking(move || read_csv_file(&path))
        .await
        .context("CSV parsing task failed")?
}

async fn inspect_event_csv_file_async(path: PathBuf) -> Result<EventCsvSummary> {
    tokio::task::spawn_blocking(move || inspect_event_csv_file(&path))
        .await
        .context("CSV validation task failed")?
}

/// Serialize one logical CSV record back to a single string. Embedded newlines
/// remain inside quoted fields; only the writer's final record terminator is
/// removed.
fn csv_record_to_string(record: &StringRecord) -> Result<String> {
    let mut bytes = Vec::new();
    {
        let mut writer = WriterBuilder::new()
            .has_headers(false)
            .from_writer(&mut bytes);
        writer.write_record(record)?;
        writer.flush()?;
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    Ok(String::from_utf8(bytes)?)
}

struct EventPage {
    header: String,
    total_events: usize,
    offset: usize,
    rows: Vec<String>,
    byte_limited: bool,
}

fn bounded_inline_header(header: String) -> Option<String> {
    let encoded_len = serde_json::to_vec(&header).ok()?.len();
    (encoded_len.saturating_add(1024) <= MAX_INLINE_EVENT_PAYLOAD_BYTES).then_some(header)
}

fn truncate_utf8(input: &str, max_bytes: usize) -> (String, bool) {
    const MARKER: &str = "…[truncated]";
    if input.len() <= max_bytes {
        return (input.to_string(), false);
    }
    if max_bytes < MARKER.len() {
        return (String::new(), true);
    }

    let mut end = max_bytes.saturating_sub(MARKER.len()).min(input.len());
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    let mut output = input[..end].to_string();
    output.push_str(MARKER);
    (output, true)
}

fn analyze_events_summary(parsed: &ParsedCsv) -> Result<Value> {
    if parsed.headers.as_slice().len() > MAX_ANALYZE_PREVIEW_ROW_BYTES {
        return Err(anyhow::anyhow!(
            "Event-list header is too large to return in the analysis preview"
        ));
    }
    let header = csv_record_to_string(&parsed.headers)?;
    let mut used_bytes = serde_json::to_vec(&header)?.len().saturating_add(256);
    let mut preview = Vec::new();
    let mut byte_limited = false;

    for record in parsed.records.iter().take(20) {
        // Check the zero-copy field backing first so a single pathological row
        // cannot cause a very large temporary JSON allocation.
        if record.as_slice().len() > MAX_ANALYZE_PREVIEW_ROW_BYTES {
            byte_limited = true;
            break;
        }
        let raw = csv_record_to_string(record)?;
        let fields: Vec<&str> = record.iter().collect();
        let item = json!({ "raw": raw, "fields": fields });
        let item_bytes = serde_json::to_vec(&item)?.len().saturating_add(1);
        if used_bytes.saturating_add(item_bytes) > MAX_ANALYZE_EVENTS_JSON_BYTES {
            byte_limited = true;
            break;
        }
        used_bytes += item_bytes;
        preview.push(item);
    }

    let preview_truncated = byte_limited || parsed.records.len() > preview.len();
    let events = json!({
        "total_events": parsed.records.len(),
        "header": header,
        "preview": preview,
        "preview_truncated": preview_truncated
    });
    if serde_json::to_vec(&events)?.len() > MAX_ANALYZE_EVENTS_JSON_BYTES {
        return Err(anyhow::anyhow!(
            "Analysis preview exceeded its {} byte inline budget",
            MAX_ANALYZE_EVENTS_JSON_BYTES
        ));
    }
    Ok(events)
}

fn event_page(parsed: &ParsedCsv, requested_offset: usize, row_limit: usize) -> Result<EventPage> {
    let header = csv_record_to_string(&parsed.headers)?;
    // Reserve space for the report's field names/counts/message, then account
    // for each JSON-escaped string exactly as serde will encode it.
    let mut used_bytes = 1024usize
        .checked_add(serde_json::to_vec(&header)?.len())
        .ok_or_else(|| anyhow::anyhow!("Inline event-list size overflow"))?;
    if used_bytes > MAX_INLINE_EVENT_PAYLOAD_BYTES {
        return Err(anyhow::anyhow!(
            "Event-list header is too large to return inline; pass output_path to save the CSV"
        ));
    }

    let total_events = parsed.records.len();
    let offset = requested_offset.min(total_events);
    let mut rows = Vec::new();
    let mut byte_limited = false;
    for record in parsed.records.iter().skip(offset).take(row_limit) {
        let row = csv_record_to_string(record)?;
        let encoded_bytes = serde_json::to_vec(&row)?.len().saturating_add(1);
        if used_bytes.saturating_add(encoded_bytes) > MAX_INLINE_EVENT_PAYLOAD_BYTES {
            if rows.is_empty() {
                return Err(anyhow::anyhow!(
                    "Event-list row {} is too large to return inline; pass output_path to save the CSV",
                    offset
                ));
            }
            byte_limited = true;
            break;
        }
        used_bytes += encoded_bytes;
        rows.push(row);
    }

    Ok(EventPage {
        header,
        total_events,
        offset,
        rows,
        byte_limited,
    })
}

/// A fresh artifact path isolated inside a randomized private directory.
/// Keeping the directory alive prevents another process from replacing the
/// unlinked child path while pixtool is producing it.
struct FreshArtifact {
    _directory: TempDir,
    path: TempPath,
}

impl Deref for FreshArtifact {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.path.as_ref()
    }
}

impl AsRef<Path> for FreshArtifact {
    fn as_ref(&self) -> &Path {
        self.path.as_ref()
    }
}

/// Reserve an isolated temporary directory on the destination filesystem,
/// then unlink its child placeholder so pixtool creates a fresh artifact.
fn fresh_temp_path(suffix: &str, destination: Option<&Path>) -> Result<FreshArtifact> {
    let directory = if let Some(destination) = destination {
        if destination.as_os_str().is_empty()
            || destination.to_string_lossy().trim().is_empty()
            || destination.file_name().is_none()
        {
            return Err(anyhow::anyhow!("Output path must name a file"));
        }
        if std::fs::symlink_metadata(destination).is_ok() && destination.is_dir() {
            return Err(anyhow::anyhow!(
                "Output path is a directory: {}",
                destination.display()
            ));
        }
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if !parent.is_dir() {
            return Err(anyhow::anyhow!(
                "Output directory does not exist or is not a directory: {}",
                parent.display()
            ));
        }
        TempFileBuilder::new()
            .prefix(".pix-mcp-artifact-")
            .tempdir_in(parent)?
    } else {
        TempFileBuilder::new()
            .prefix("pix-mcp-artifact-")
            .tempdir()?
    };

    let file = TempFileBuilder::new()
        .prefix("output-")
        .suffix(suffix)
        .tempfile_in(directory.path())?;
    let path = file.into_temp_path();
    std::fs::remove_file(&path)?;
    Ok(FreshArtifact {
        _directory: directory,
        path,
    })
}

fn persist_temp_path(
    artifact: FreshArtifact,
    destination: &Path,
    replace_existing: bool,
) -> Result<()> {
    let result = if replace_existing {
        artifact.path.persist(destination)
    } else {
        artifact.path.persist_noclobber(destination)
    };
    result.map_err(|error| {
        anyhow::anyhow!(
            "Failed to {} output file {}: {}",
            if replace_existing {
                "replace"
            } else {
                "create"
            },
            destination.display(),
            error
        )
    })
}

async fn fresh_temp_path_async(
    suffix: &'static str,
    destination: Option<PathBuf>,
) -> Result<FreshArtifact> {
    tokio::task::spawn_blocking(move || fresh_temp_path(suffix, destination.as_deref()))
        .await
        .context("Temporary output preparation task failed")?
}

async fn persist_temp_path_async(
    artifact: FreshArtifact,
    destination: PathBuf,
    replace_existing: bool,
) -> Result<()> {
    tokio::task::spawn_blocking(move || persist_temp_path(artifact, &destination, replace_existing))
        .await
        .context("Output persistence task failed")?
}

fn decode_png_file(path: &Path) -> Result<(u64, image::DynamicImage)> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("PNG output was not created: {}", path.display()))?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(anyhow::anyhow!(
            "PNG output is missing or empty: {}",
            path.display()
        ));
    }

    let mut reader = image::ImageReader::open(path)?
        .with_guessed_format()
        .context("Could not determine screenshot image format")?;
    if reader.format() != Some(image::ImageFormat::Png) {
        return Err(anyhow::anyhow!(
            "pixtool output is not a PNG image: {}",
            path.display()
        ));
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SCREENSHOT_SOURCE_DIMENSION);
    limits.max_image_height = Some(MAX_SCREENSHOT_SOURCE_DIMENSION);
    limits.max_alloc = Some(MAX_SCREENSHOT_DECODE_BYTES);
    reader.limits(limits);
    let image = reader
        .decode()
        .with_context(|| format!("pixtool produced an invalid PNG: {}", path.display()))?;
    Ok((metadata.len(), image))
}

#[cfg(test)]
fn validate_png_file(path: &Path) -> Result<u64> {
    decode_png_file(path).map(|(size, _)| size)
}

fn validate_screenshot_options(
    depth: bool,
    marker: Option<&str>,
    global_id: Option<u64>,
    rtv_index: Option<u64>,
    embed_image: bool,
    max_dimension: u32,
) -> Result<()> {
    if embed_image && !(1..=SCREENSHOT_MAX_DIMENSION).contains(&max_dimension) {
        return Err(anyhow::anyhow!(
            "max_dimension must be between 1 and {} pixels",
            SCREENSHOT_MAX_DIMENSION
        ));
    }
    if depth && rtv_index.is_some() {
        return Err(anyhow::anyhow!(
            "depth and rtv_index are mutually exclusive resource selectors"
        ));
    }
    if rtv_index.is_some_and(|index| index > 7) {
        return Err(anyhow::anyhow!("rtv_index must be between 0 and 7"));
    }
    if marker.is_some() && global_id.is_some() {
        return Err(anyhow::anyhow!(
            "marker and global_id are mutually exclusive event selectors"
        ));
    }
    if marker.is_some_and(|value| value.trim().is_empty()) {
        return Err(anyhow::anyhow!("marker must not be empty"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Argument types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportCountersArgs {
    /// Path to the PIX-exported counters file (CSV or JSON).
    pub file_path: String,
    /// File format: "csv", "json", or "auto" (detected from the extension).
    #[serde(default)]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompareCapturesArgs {
    /// Path to the first capture file (baseline).
    pub capture_a: String,
    /// Path to the second capture file (comparison).
    pub capture_b: String,
    /// Include all available counters in each event export. This can make
    /// replay substantially slower; structural comparison does not require it.
    #[serde(default)]
    pub include_counters: Option<bool>,
    /// Maximum event rows retained from each capture for structural analysis.
    /// The full CSV is still counted, so truncation is reported explicitly.
    #[serde(default)]
    #[schemars(range(min = 1, max = 50000))]
    pub max_events: Option<usize>,
    /// Maximum changed event kinds returned (default 20, maximum 50).
    #[serde(default)]
    #[schemars(range(min = 1, max = 50))]
    pub max_changes: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeCaptureArgs {
    /// Path to the .wpix capture file.
    pub capture_path: String,
    /// Include performance counters (slower but more detailed).
    #[serde(default)]
    pub include_counters: Option<bool>,
    /// Counter name pattern, e.g. "*" (all), "D3D*". Passed to pixtool
    /// `save-event-list --counters=<pattern>`.
    #[serde(default)]
    pub counter_pattern: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventListArgs {
    /// Path to the .wpix capture file.
    pub capture_path: String,
    /// Path to save the full CSV output. When set, the file is written and a
    /// path is returned instead of inlining rows (token-efficient for big lists).
    #[serde(default)]
    pub output_path: Option<String>,
    /// Counter name pattern to include, e.g. "*" (all), "D3D*". Passed to
    /// pixtool `save-event-list --counters=<pattern>`.
    #[serde(default)]
    pub counters: Option<String>,
    /// Inline detail level when no output_path is given.
    #[serde(default)]
    pub response_format: Option<ResponseFormat>,
    /// Row offset for inline pagination (default 0).
    #[serde(default)]
    pub offset: Option<usize>,
    /// Max rows to return inline (capped at 2000).
    #[serde(default)]
    #[schemars(range(min = 1, max = 2000))]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScreenshotArgs {
    /// Path to the .wpix capture file.
    pub capture_path: String,
    /// Path to save the PNG screenshot. If omitted, the server may ask for it.
    #[serde(default)]
    pub output_path: Option<String>,
    /// Save the depth buffer instead of the color screenshot (uses pixtool
    /// `save-resource --depth`, which replays the capture). Default: false.
    #[serde(default)]
    pub depth: Option<bool>,
    /// Save the resource bound under a named PIX marker region instead of the
    /// recorded screenshot (uses `save-resource --marker=<name>`).
    #[serde(default)]
    pub marker: Option<String>,
    /// Save the resource bound at a specific draw's Global ID (uses
    /// `save-resource --global-id=<id>`). Get IDs from pix_get_event_list.
    #[serde(default)]
    pub global_id: Option<u64>,
    /// RenderTargetView index to save when a draw binds multiple RTVs (uses
    /// `save-resource --rtv=<index>`). Only applies on the save-resource path.
    #[serde(default)]
    #[schemars(range(max = 7))]
    pub rtv_index: Option<u64>,
    /// Embed the image inline so a vision model can see it (default: true).
    #[serde(default)]
    pub embed_image: Option<bool>,
    /// Max width/height for the inline thumbnail in pixels (default: 1280).
    #[serde(default)]
    #[schemars(range(min = 1, max = 4096))]
    pub max_dimension: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CapturePathArgs {
    /// Path to the .wpix capture file.
    pub capture_path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListCountersArgs {
    /// Path to the .wpix capture file.
    pub capture_path: String,
    /// Case-insensitive substring filter applied to counter names.
    #[serde(default)]
    pub filter: Option<String>,
    /// Max counters to return (default 200).
    #[serde(default)]
    #[schemars(range(min = 1, max = 2000))]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeFrameArgs {
    /// Path to the .wpix capture file.
    pub capture_path: String,
    /// Include GPU timing counters so the most expensive events can be ranked.
    #[serde(default)]
    pub include_counters: Option<bool>,
}

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExportCountersReport {
    pub success: bool,
    pub format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_count: Option<usize>,
    pub data: Value,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CaptureSide {
    pub path: String,
    pub size_bytes: u64,
    pub modified: Option<u64>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ComparisonDetail {
    pub size_difference_bytes: i64,
    pub size_difference_percent: String,
    pub size_increased: bool,
    pub note: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EventCountChange {
    pub queue_id: String,
    pub event_name: String,
    pub baseline_count: usize,
    pub comparison_count: usize,
    pub difference: i64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EventSequenceComparison {
    pub compared_positions: usize,
    pub exact_position_matches: usize,
    pub exact_position_match_percent: String,
    pub common_prefix_events: usize,
    /// Only meaningful when neither retained event list was truncated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub common_suffix_events: Option<usize>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EventColumnComparison {
    pub added_count: usize,
    pub removed_count: usize,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventComparisonScope {
    FullCapture,
    Prefix,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EventStructureComparison {
    pub scope: EventComparisonScope,
    pub baseline_total_events: usize,
    pub comparison_total_events: usize,
    pub total_event_difference: i64,
    pub baseline_analyzed_events: usize,
    pub comparison_analyzed_events: usize,
    pub truncated: bool,
    pub sequence: EventSequenceComparison,
    pub columns: EventColumnComparison,
    pub changed_event_kinds: usize,
    pub changes_truncated: bool,
    pub top_changes: Vec<EventCountChange>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TimingAggregate {
    pub samples: usize,
    pub sum: f64,
    pub mean: f64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct GpuTimingComparison {
    pub column: String,
    pub baseline: TimingAggregate,
    pub comparison: TimingAggregate,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sum_difference_percent: Option<String>,
    pub note: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CompareAnalysisMode {
    EventStructure,
    MetadataOnly,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CompareReport {
    pub success: bool,
    pub capture_a: CaptureSide,
    pub capture_b: CaptureSide,
    pub comparison: ComparisonDetail,
    pub analysis_mode: CompareAnalysisMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_structure: Option<EventStructureComparison>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_timing: Option<GpuTimingComparison>,
    pub warnings: Vec<String>,
    pub suggestion: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct AnalyzeReport {
    pub success: bool,
    pub capture_path: String,
    pub file_size_bytes: u64,
    pub file_size_mb: String,
    pub events: Value,
    pub pixtool_output: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EventListReport {
    pub success: bool,
    /// Set when the event list was saved to a file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Total number of events in the capture.
    pub total_events: usize,
    /// Inline pagination: starting row offset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    /// Inline pagination: number of rows returned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub returned: Option<usize>,
    /// Whether more rows are available beyond this page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    /// Offset to pass next to continue paging, if truncated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    /// CSV header line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// The requested slice of CSV rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<Vec<String>>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ScreenshotReport {
    pub success: bool,
    pub output_path: String,
    pub file_size_bytes: u64,
    pub message: String,
    /// Whether the image was embedded inline as image content.
    pub image_embedded: bool,
}

/// Screenshot report plus the optional inline image (base64 PNG).
pub struct ScreenshotResult {
    pub report: ScreenshotReport,
    pub image_b64: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CountersReport {
    pub success: bool,
    pub capture_path: String,
    /// Total counters available (before filtering).
    pub total_count: usize,
    /// Number returned after filtering/limiting.
    pub returned: usize,
    /// Whether matching counters were omitted by the item or byte limit.
    pub truncated: bool,
    pub counters: Vec<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct AnalysisDetail {
    pub error_count: usize,
    pub warning_count: usize,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RunAnalysisReport {
    pub success: bool,
    pub capture_path: String,
    pub analysis: AnalysisDetail,
    pub process_exit_code: Option<i32>,
    /// Important limitation of pixtool's headless debug-layer command.
    pub note: String,
    pub full_output: String,
}

/// Heuristic, AI-oriented summary of a captured frame.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FrameInsights {
    pub success: bool,
    pub capture_path: String,
    pub total_events: usize,
    pub draw_calls: usize,
    pub dispatches: usize,
    pub resource_barriers: usize,
    pub render_target_changes: usize,
    pub copies: usize,
    /// Event-list column headers (for reference).
    pub event_columns: Vec<String>,
    /// Most expensive events by GPU time, if a timing column was available.
    pub top_events: Vec<Value>,
    /// Human-readable observations and caveats.
    pub notes: Vec<String>,
}

// ---------------------------------------------------------------------------
// Counter export
// ---------------------------------------------------------------------------

pub async fn handle_pix_export_counters(args: ExportCountersArgs) -> Result<ExportCountersReport> {
    tokio::task::spawn_blocking(move || export_counters(args))
        .await
        .context("Counter export parsing task failed")?
}

fn export_counters(args: ExportCountersArgs) -> Result<ExportCountersReport> {
    let path = security::validate_input_file(Path::new(&args.file_path), "Counter export")?;
    let bytes = read_file_limited(&path, MAX_COUNTER_EXPORT_BYTES, "Counter export")?;
    let content = String::from_utf8(bytes).context("Counter export is not valid UTF-8")?;

    let format = args
        .format
        .as_deref()
        .unwrap_or("auto")
        .trim()
        .to_ascii_lowercase();
    let detected_format = if format == "auto" {
        match path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("csv") => "csv",
            Some("json") => "json",
            _ => {
                return Err(anyhow::anyhow!(
                    "Cannot detect format from extension. Pass format: \"csv\" or \"json\"."
                ));
            }
        }
    } else {
        format.as_str()
    };

    match detected_format {
        "csv" => parse_csv_counters(&content),
        "json" => parse_json_counters(&content),
        other => Err(anyhow::anyhow!(
            "Unsupported format \"{}\". Use \"csv\" or \"json\".",
            other
        )),
    }
}

fn parse_csv_counters(content: &str) -> Result<ExportCountersReport> {
    let parsed = parse_csv_bytes_with_limit(content.as_bytes(), MAX_COUNTER_EXPORT_ROWS)?;
    let columns: Vec<String> = parsed
        .headers
        .iter()
        .map(|column| column.trim().to_string())
        .collect();
    let mut unique_columns = std::collections::HashSet::new();
    for column in &columns {
        if column.is_empty() {
            return Err(anyhow::anyhow!("CSV contains an empty column name"));
        }
        if !unique_columns.insert(column.to_lowercase()) {
            return Err(anyhow::anyhow!(
                "CSV contains a duplicate column name: {}",
                column
            ));
        }
    }

    let mut rows: Vec<Value> = Vec::new();
    for record in parsed.records {
        if record.is_empty() || record.iter().all(|value| value.trim().is_empty()) {
            continue;
        }
        let mut row = serde_json::Map::new();
        for (i, col) in columns.iter().enumerate() {
            if let Some(value) = record.get(i) {
                let val = value.trim();
                if let Ok(number) = val.parse::<i64>() {
                    row.insert(col.clone(), json!(number));
                } else if let Ok(number) = val.parse::<u64>() {
                    row.insert(col.clone(), json!(number));
                } else if let Ok(number) = val.parse::<f64>() {
                    if number.is_finite() {
                        row.insert(col.clone(), json!(number));
                    } else {
                        row.insert(col.clone(), json!(val));
                    }
                } else {
                    row.insert(col.clone(), json!(val));
                }
            }
        }
        rows.push(Value::Object(row));
    }

    let row_count = rows.len();
    let data = Value::Array(rows);
    let serialized_size = serde_json::to_vec(&data)?.len();
    if serialized_size > MAX_COUNTER_RESULT_BYTES {
        return Err(anyhow::anyhow!(
            "Parsed counter data is too large to return ({} bytes; maximum {}). Export a smaller selection.",
            serialized_size,
            MAX_COUNTER_RESULT_BYTES
        ));
    }

    Ok(ExportCountersReport {
        success: true,
        format: "csv".to_string(),
        columns: Some(columns),
        row_count: Some(row_count),
        data,
    })
}

fn parse_json_counters(content: &str) -> Result<ExportCountersReport> {
    let data: Value = serde_json::from_str(content)?;
    let serialized_size = serde_json::to_vec(&data)?.len();
    if serialized_size > MAX_COUNTER_RESULT_BYTES {
        return Err(anyhow::anyhow!(
            "Parsed counter data is too large to return ({} bytes; maximum {}). Export a smaller selection.",
            serialized_size,
            MAX_COUNTER_RESULT_BYTES
        ));
    }
    Ok(ExportCountersReport {
        success: true,
        format: "json".to_string(),
        columns: None,
        row_count: None,
        data,
    })
}

// ---------------------------------------------------------------------------
// Compare
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ComparisonCsv {
    headers: StringRecord,
    events: Vec<ComparisonEvent>,
    total_events: usize,
    gpu_duration_column: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EventIdentity {
    queue_id: String,
    name: String,
}

#[derive(Debug)]
struct ComparisonEvent {
    identity: EventIdentity,
    gpu_duration: Option<f64>,
}

fn parse_comparison_csv_reader(reader: impl Read, max_events: usize) -> Result<ComparisonCsv> {
    let mut reader = ReaderBuilder::new().flexible(false).from_reader(reader);
    let headers = reader
        .headers()
        .context("Invalid event-list CSV header")?
        .clone();
    validate_event_list_headers(&headers)?;
    if headers.len() > MAX_COMPARE_COLUMNS
        || headers.as_slice().len() > MAX_COMPARE_HEADER_BYTES
        || headers
            .iter()
            .any(|header| header.len() > MAX_COMPARE_EVENT_FIELD_BYTES)
    {
        return Err(anyhow::anyhow!(
            "Event-list header exceeds comparison limits ({} columns, {} total bytes, {} bytes per column)",
            MAX_COMPARE_COLUMNS,
            MAX_COMPARE_HEADER_BYTES,
            MAX_COMPARE_EVENT_FIELD_BYTES,
        ));
    }
    let queue_column = event_column(&headers, "Queue ID")?;
    let name_column = event_column(&headers, "Name")?;
    let gpu_duration = gpu_duration_column(&headers);

    let mut events = Vec::with_capacity(max_events.min(4_096));
    let mut total_events = 0usize;
    for record in reader.records() {
        if total_events >= MAX_ANALYSIS_CSV_ROWS {
            return Err(anyhow::anyhow!(
                "Event-list CSV contains more than the supported maximum of {} rows",
                MAX_ANALYSIS_CSV_ROWS
            ));
        }
        let record = record.with_context(|| {
            format!("Invalid event-list CSV record at row {}", total_events + 1)
        })?;
        total_events += 1;
        if events.len() < max_events {
            let queue_id = record.get(queue_column).unwrap_or_default().trim();
            let name = record.get(name_column).unwrap_or_default().trim();
            if queue_id.len() > MAX_COMPARE_EVENT_FIELD_BYTES
                || name.len() > MAX_COMPARE_EVENT_FIELD_BYTES
            {
                return Err(anyhow::anyhow!(
                    "Event {} contains a queue/name field larger than the comparison limit of {} bytes",
                    total_events,
                    MAX_COMPARE_EVENT_FIELD_BYTES
                ));
            }
            let duration = gpu_duration
                .as_ref()
                .and_then(|(index, _)| record.get(*index))
                .and_then(|value| value.trim().parse::<f64>().ok())
                .filter(|value| value.is_finite());
            events.push(ComparisonEvent {
                identity: EventIdentity {
                    queue_id: queue_id.to_string(),
                    name: name.to_string(),
                },
                gpu_duration: duration,
            });
        }
    }
    if total_events == 0 {
        return Err(anyhow::anyhow!("Event-list CSV contains no events"));
    }
    Ok(ComparisonCsv {
        headers,
        events,
        total_events,
        gpu_duration_column: gpu_duration.map(|(_, name)| name),
    })
}

#[cfg(test)]
fn parse_comparison_csv_bytes(bytes: &[u8], max_events: usize) -> Result<ComparisonCsv> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    if bytes.is_empty() {
        return Err(anyhow::anyhow!("Empty event-list CSV"));
    }
    parse_comparison_csv_reader(bytes, max_events)
}

fn read_comparison_csv(path: &Path, max_events: usize) -> Result<ComparisonCsv> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("Cannot inspect comparison event list: {}", path.display()))?;
    if metadata.len() == 0 {
        return Err(anyhow::anyhow!("Empty event-list CSV"));
    }
    if metadata.len() > MAX_ANALYSIS_CSV_BYTES {
        return Err(anyhow::anyhow!(
            "Comparison event list exceeds the {} byte limit",
            MAX_ANALYSIS_CSV_BYTES
        ));
    }
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("Cannot open comparison event list: {}", path.display()))?;
    let mut prefix = [0u8; 3];
    let read = file
        .read(&mut prefix)
        .context("Could not inspect comparison CSV prefix")?;
    let start = if read == prefix.len() && prefix == [0xEF, 0xBB, 0xBF] {
        3
    } else {
        0
    };
    file.seek(SeekFrom::Start(start))
        .context("Could not rewind comparison CSV")?;
    parse_comparison_csv_reader(BufReader::new(file), max_events)
}

async fn export_comparison_events(
    capture_path: PathBuf,
    max_events: usize,
    include_counters: bool,
) -> Result<ComparisonCsv> {
    let csv = fresh_temp_path_async(".csv", None).await?;
    let pixtool = PixTool::find()?;
    let mut command = Command::new(pixtool);
    command
        .arg("open-capture")
        .arg(&capture_path)
        .arg("save-event-list")
        .arg(csv.as_os_str());
    if include_counters {
        push_value_option(&mut command, "--counters", "*")?;
    }
    let output =
        run_pixtool_command(command, ANALYSIS_TIMEOUT, "pixtool capture comparison").await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    validate_artifact_command_output(
        output.status.success(),
        output.status.code(),
        &stdout,
        &stderr,
        "capture comparison",
    )?;
    let csv_path = csv.to_path_buf();
    tokio::task::spawn_blocking(move || read_comparison_csv(&csv_path, max_events))
        .await
        .context("Comparison event-list parsing task failed")?
}

fn event_column(headers: &StringRecord, expected: &str) -> Result<usize> {
    headers
        .iter()
        .position(|header| header.trim().eq_ignore_ascii_case(expected))
        .ok_or_else(|| anyhow::anyhow!("PIX event list is missing the {expected:?} column"))
}

fn event_identities(csv: &ComparisonCsv) -> Vec<EventIdentity> {
    csv.events
        .iter()
        .map(|event| event.identity.clone())
        .collect()
}

fn compare_columns(a: &StringRecord, b: &StringRecord) -> EventColumnComparison {
    const MAX_RETURNED_COLUMNS: usize = 50;
    let normalize = |headers: &StringRecord| {
        headers
            .iter()
            .map(|header| {
                (
                    header.trim().to_ascii_lowercase(),
                    header.trim().to_string(),
                )
            })
            .collect::<BTreeMap<_, _>>()
    };
    let a = normalize(a);
    let b = normalize(b);
    let all_added = b
        .iter()
        .filter(|(normalized, _)| !a.contains_key(*normalized))
        .map(|(_, display)| display.clone())
        .collect::<Vec<_>>();
    let all_removed = a
        .iter()
        .filter(|(normalized, _)| !b.contains_key(*normalized))
        .map(|(_, display)| display.clone())
        .collect::<Vec<_>>();
    EventColumnComparison {
        added_count: all_added.len(),
        removed_count: all_removed.len(),
        added: all_added
            .iter()
            .take(MAX_RETURNED_COLUMNS)
            .cloned()
            .collect(),
        removed: all_removed
            .iter()
            .take(MAX_RETURNED_COLUMNS)
            .cloned()
            .collect(),
        truncated: all_added.len() > MAX_RETURNED_COLUMNS
            || all_removed.len() > MAX_RETURNED_COLUMNS,
    }
}

fn gpu_duration_column(headers: &StringRecord) -> Option<(usize, String)> {
    headers.iter().enumerate().find_map(|(index, header)| {
        let normalized = header.trim().to_ascii_lowercase();
        let recognized = ["gpu duration", "gpu time"].iter().any(|base| {
            normalized == *base
                || normalized.strip_prefix(base).is_some_and(|suffix| {
                    let suffix = suffix.trim_start();
                    suffix.starts_with('(') || suffix.starts_with('[')
                })
        });
        recognized.then(|| (index, header.trim().to_string()))
    })
}

fn timing_aggregate(csv: &ComparisonCsv) -> Option<TimingAggregate> {
    let mut samples = 0usize;
    let mut sum = 0.0f64;
    for event in &csv.events {
        if let Some(value) = event.gpu_duration {
            samples += 1;
            sum += value;
        }
    }
    (samples > 0 && sum.is_finite()).then(|| TimingAggregate {
        samples,
        sum,
        mean: sum / samples as f64,
    })
}

fn compare_gpu_timing(a: &ComparisonCsv, b: &ComparisonCsv) -> Option<GpuTimingComparison> {
    if a.events.len() != a.total_events || b.events.len() != b.total_events {
        return None;
    }
    let name = a.gpu_duration_column.as_ref()?;
    if !b
        .gpu_duration_column
        .as_ref()
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
    {
        return None;
    }
    let baseline = timing_aggregate(a)?;
    let comparison = timing_aggregate(b)?;
    let sum_difference_percent = (baseline.sum != 0.0).then(|| {
        format!(
            "{:.2}%",
            ((comparison.sum - baseline.sum) / baseline.sum) * 100.0
        )
    });
    Some(GpuTimingComparison {
        column: name.clone(),
        baseline,
        comparison,
        sum_difference_percent,
        note: "This is an aggregate of complete event exports using the same recognized GPU duration column. Nested PIX events can overlap or double-count time, so it is a triage signal rather than a frame-time verdict."
            .to_string(),
    })
}

fn build_event_structure_comparison(
    a: &ComparisonCsv,
    b: &ComparisonCsv,
    max_changes: usize,
) -> Result<EventStructureComparison> {
    let events_a = event_identities(a);
    let events_b = event_identities(b);
    let truncated = a.events.len() < a.total_events || b.events.len() < b.total_events;
    let compared_positions = events_a.len().min(events_b.len());
    let exact_position_matches = events_a
        .iter()
        .zip(&events_b)
        .filter(|(left, right)| left == right)
        .count();
    let common_prefix_events = events_a
        .iter()
        .zip(&events_b)
        .take_while(|(left, right)| left == right)
        .count();
    let common_suffix_events = (!truncated).then(|| {
        events_a
            .iter()
            .rev()
            .zip(events_b.iter().rev())
            .take(compared_positions.saturating_sub(common_prefix_events))
            .take_while(|(left, right)| left == right)
            .count()
    });

    let mut counts = HashMap::<EventIdentity, (usize, usize)>::new();
    for event in events_a {
        counts.entry(event).or_default().0 += 1;
    }
    for event in events_b {
        counts.entry(event).or_default().1 += 1;
    }
    let mut changes = counts
        .into_iter()
        .filter(|(_, (baseline, comparison))| baseline != comparison)
        .map(
            |(event, (baseline_count, comparison_count))| EventCountChange {
                queue_id: event.queue_id,
                event_name: event.name,
                baseline_count,
                comparison_count,
                difference: comparison_count as i64 - baseline_count as i64,
            },
        )
        .collect::<Vec<_>>();
    changes.sort_by(|left, right| {
        right
            .difference
            .unsigned_abs()
            .cmp(&left.difference.unsigned_abs())
            .then_with(|| left.queue_id.cmp(&right.queue_id))
            .then_with(|| left.event_name.cmp(&right.event_name))
    });
    let changed_event_kinds = changes.len();
    changes.truncate(max_changes);

    Ok(EventStructureComparison {
        scope: if truncated {
            EventComparisonScope::Prefix
        } else {
            EventComparisonScope::FullCapture
        },
        baseline_total_events: a.total_events,
        comparison_total_events: b.total_events,
        total_event_difference: b.total_events as i64 - a.total_events as i64,
        baseline_analyzed_events: a.events.len(),
        comparison_analyzed_events: b.events.len(),
        truncated,
        sequence: EventSequenceComparison {
            compared_positions,
            exact_position_matches,
            exact_position_match_percent: if compared_positions == 0 {
                "0.00%".to_string()
            } else {
                format!(
                    "{:.2}%",
                    exact_position_matches as f64 / compared_positions as f64 * 100.0
                )
            },
            common_prefix_events,
            common_suffix_events,
        },
        columns: compare_columns(&a.headers, &b.headers),
        changed_event_kinds,
        changes_truncated: changed_event_kinds > changes.len(),
        top_changes: changes,
    })
}

fn metadata_comparison(size_a: u64, size_b: u64) -> ComparisonDetail {
    let size_diff_exact = i128::from(size_b) - i128::from(size_a);
    let size_difference_bytes =
        size_diff_exact.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
    let size_difference_percent = if size_a > 0 {
        (size_diff_exact as f64 / size_a as f64) * 100.0
    } else {
        0.0
    };
    ComparisonDetail {
        size_difference_bytes,
        size_difference_percent: format!("{size_difference_percent:.2}%"),
        size_increased: size_difference_bytes > 0,
        note: "File size is retained as context only; event structure and like-for-like counters are the meaningful comparison signals."
            .to_string(),
    }
}

fn capture_side(path: String, metadata: &std::fs::Metadata) -> CaptureSide {
    CaptureSide {
        path,
        size_bytes: metadata.len(),
        modified: metadata.modified().ok().map(|time| {
            time.duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0)
        }),
    }
}

fn bounded_compare_warning(prefix: &str, error: &anyhow::Error) -> String {
    let warning = format!("{prefix}: {error}");
    truncate_utf8(&warning, MAX_COMPARE_WARNING_BYTES).0
}

fn fit_compare_report(mut report: CompareReport) -> Result<CompareReport> {
    loop {
        if serde_json::to_vec(&report)?.len() <= MAX_COMPARE_REPORT_BYTES {
            return Ok(report);
        }
        if let Some(structure) = report.event_structure.as_mut() {
            if structure.top_changes.pop().is_some() {
                structure.changes_truncated = true;
                continue;
            }
            if structure.columns.added.pop().is_some() || structure.columns.removed.pop().is_some()
            {
                structure.columns.truncated = true;
                continue;
            }
        }
        if report.gpu_timing.take().is_some() {
            report.warnings.push(
                "GPU timing aggregate was omitted to keep the comparison response within 1 MiB."
                    .to_string(),
            );
            continue;
        }
        return Err(anyhow::anyhow!(
            "Capture comparison exceeded its {} byte inline response budget",
            MAX_COMPARE_REPORT_BYTES
        ));
    }
}

pub async fn handle_pix_compare_captures(args: CompareCapturesArgs) -> Result<CompareReport> {
    let max_events = args.max_events.unwrap_or(COMPARE_DEFAULT_MAX_EVENTS);
    if !(1..=COMPARE_MAX_EVENTS).contains(&max_events) {
        return Err(anyhow::anyhow!(
            "max_events must be between 1 and {COMPARE_MAX_EVENTS}"
        ));
    }
    let max_changes = args.max_changes.unwrap_or(COMPARE_DEFAULT_MAX_CHANGES);
    if !(1..=COMPARE_MAX_CHANGES).contains(&max_changes) {
        return Err(anyhow::anyhow!(
            "max_changes must be between 1 and {COMPARE_MAX_CHANGES}"
        ));
    }
    let include_counters = args.include_counters.unwrap_or(false);

    let (path_a, path_b) = tokio::try_join!(
        require_capture_file_async(args.capture_a.clone()),
        require_capture_file_async(args.capture_b.clone())
    )?;
    if path_a == path_b {
        return Err(anyhow::anyhow!(
            "capture_a and capture_b must identify different capture files"
        ));
    }
    let metadata_a = std::fs::metadata(&path_a)?;
    let metadata_b = std::fs::metadata(&path_b)?;
    let comparison = metadata_comparison(metadata_a.len(), metadata_b.len());
    let capture_a = capture_side(args.capture_a, &metadata_a);
    let capture_b = capture_side(args.capture_b, &metadata_b);

    let mut warnings = Vec::new();
    let (events_a, events_b, counters_available) = if include_counters {
        // Counter replay is deliberately serialized: replaying two captures on
        // the same GPU at once can contaminate measurements and monopolize the
        // complete pixtool foreground pool.
        let counter_a = export_comparison_events(path_a.clone(), max_events, true).await;
        let counter_b = export_comparison_events(path_b.clone(), max_events, true).await;
        match (counter_a, counter_b) {
            (Ok(events_a), Ok(events_b)) => (Ok(events_a), Ok(events_b), true),
            (counter_a, counter_b) => {
                if let Err(error) = &counter_a {
                    warnings.push(bounded_compare_warning(
                        "Baseline counter export failed",
                        error,
                    ));
                }
                if let Err(error) = &counter_b {
                    warnings.push(bounded_compare_warning(
                        "Comparison counter export failed",
                        error,
                    ));
                }
                warnings.push(
                    "Retrying both event exports without counters so structural comparison remains available."
                        .to_string(),
                );
                let (events_a, events_b) = tokio::join!(
                    export_comparison_events(path_a, max_events, false),
                    export_comparison_events(path_b, max_events, false)
                );
                (events_a, events_b, false)
            }
        }
    } else {
        let (events_a, events_b) = tokio::join!(
            export_comparison_events(path_a, max_events, false),
            export_comparison_events(path_b, max_events, false)
        );
        (events_a, events_b, false)
    };
    let mut event_structure = None;
    let mut gpu_timing = None;

    match (events_a, events_b) {
        (Ok(events_a), Ok(events_b)) => {
            match build_event_structure_comparison(&events_a, &events_b, max_changes) {
                Ok(structure) => {
                    if structure.truncated {
                        warnings.push(format!(
                            "Structural counts and sequence matching are limited to the first {max_events} events of each capture; total event counts cover the full bounded CSV exports."
                        ));
                    }
                    event_structure = Some(structure);
                    if counters_available {
                        gpu_timing = compare_gpu_timing(&events_a, &events_b);
                    }
                    if include_counters && counters_available && gpu_timing.is_none() {
                        warnings.push(
                            "Counters were exported, but no timing aggregate is reported: a complete event set and the same recognized numeric GPU Duration/GPU Time column are required."
                                .to_string(),
                        );
                    } else if include_counters && !counters_available {
                        warnings.push(
                            "GPU timing comparison is unavailable because counter export failed; structural results come from the counter-free retry."
                                .to_string(),
                        );
                    }
                }
                Err(error) => warnings.push(bounded_compare_warning(
                    "Event exports succeeded but structural comparison was unavailable",
                    &error,
                )),
            }
        }
        (Err(error_a), Err(error_b)) => {
            warnings.push(bounded_compare_warning(
                "Baseline event export failed",
                &error_a,
            ));
            warnings.push(bounded_compare_warning(
                "Comparison event export failed",
                &error_b,
            ));
        }
        (Err(error), Ok(_)) => warnings.push(bounded_compare_warning(
            "Baseline event export failed",
            &error,
        )),
        (Ok(_), Err(error)) => warnings.push(bounded_compare_warning(
            "Comparison event export failed",
            &error,
        )),
    }

    let analysis_mode = if event_structure.is_some() {
        CompareAnalysisMode::EventStructure
    } else {
        CompareAnalysisMode::MetadataOnly
    };
    fit_compare_report(CompareReport {
        success: true,
        capture_a,
        capture_b,
        comparison,
        analysis_mode,
        event_structure,
        gpu_timing,
        warnings,
        suggestion: if analysis_mode == CompareAnalysisMode::EventStructure {
            "Use top_changes to locate added/removed work. Treat GPU timing aggregates as triage only, then verify the same marker/counter range in PIX."
                .to_string()
        } else {
            "Only metadata could be compared. Enable Windows Developer Mode, verify capture replay compatibility, and retry event export."
                .to_string()
        },
    })
}

// ---------------------------------------------------------------------------
// Analyze (event preview)
// ---------------------------------------------------------------------------

pub async fn handle_pix_analyze_capture(args: AnalyzeCaptureArgs) -> Result<AnalyzeReport> {
    let path = require_capture_file_async(args.capture_path.clone()).await?;

    let include_counters = args.include_counters.unwrap_or(false);
    let counter_pattern = args.counter_pattern.as_deref().unwrap_or("*");
    if include_counters && counter_pattern.trim().is_empty() {
        return Err(anyhow::anyhow!("counter_pattern must not be empty"));
    }

    let event_csv = fresh_temp_path_async(".csv", None).await?;

    let pixtool = PixTool::find()?;
    let mut cmd = Command::new(&pixtool);
    cmd.arg("open-capture").arg(&path);
    cmd.arg("save-event-list").arg(event_csv.as_os_str());
    if include_counters {
        push_value_option(&mut cmd, "--counters", counter_pattern)?;
    }

    let output = run_pixtool_command(cmd, ANALYSIS_TIMEOUT, "pixtool capture analysis").await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    validate_artifact_command_output(
        output.status.success(),
        output.status.code(),
        &stdout,
        &stderr,
        "capture analysis",
    )?;
    let parsed = read_csv_file_async(event_csv.to_path_buf())
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "Failed to extract a valid event list: {}\nstdout: {}\nstderr: {}",
                error,
                public_process_output(&stdout),
                public_process_output(&stderr)
            )
        })?;
    let events = analyze_events_summary(&parsed)?;

    let metadata = std::fs::metadata(&path)?;
    let public_stdout = public_process_output(&stdout);
    let (pixtool_output, _) = truncate_utf8(&public_stdout, MAX_INLINE_PROCESS_OUTPUT_BYTES);
    let report = AnalyzeReport {
        // PIX's documented spurious -1 status is accepted only when the CSV is
        // valid and the process emitted no explicit failure diagnostics.
        success: true,
        capture_path: path.to_string_lossy().into_owned(),
        file_size_bytes: metadata.len(),
        file_size_mb: format!("{:.2}", metadata.len() as f64 / 1_048_576.0),
        events,
        pixtool_output,
    };
    if serde_json::to_vec(&report)?.len() > MAX_INLINE_ANALYSIS_PAYLOAD_BYTES {
        return Err(anyhow::anyhow!(
            "Analysis report exceeded its {} byte inline budget; use pix_get_event_list with output_path for the full CSV",
            MAX_INLINE_ANALYSIS_PAYLOAD_BYTES
        ));
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// Event list (paginated / token-efficient)
// ---------------------------------------------------------------------------

pub async fn handle_pix_get_event_list(args: EventListArgs) -> Result<EventListReport> {
    let capture_path = require_capture_file_async(args.capture_path.clone()).await?;
    let format = args.response_format.unwrap_or(ResponseFormat::Summary);
    let limit = event_list_limit(args.limit, format)?;

    let user_output = event_output_path(args.output_path.as_deref())?;
    let csv_path = fresh_temp_path_async(".csv", user_output.clone()).await?;

    let pixtool = PixTool::find()?;
    let mut cmd = Command::new(&pixtool);
    cmd.arg("open-capture").arg(&capture_path);
    cmd.arg("save-event-list").arg(csv_path.as_os_str());
    if let Some(pattern) = args.counters.as_deref() {
        if pattern.trim().is_empty() {
            return Err(anyhow::anyhow!("counters must not be empty"));
        }
        push_value_option(&mut cmd, "--counters", pattern)?;
    }

    let output = run_pixtool_command(cmd, ANALYSIS_TIMEOUT, "pixtool event-list export").await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    validate_artifact_command_output(
        output.status.success(),
        output.status.code(),
        &stdout,
        &stderr,
        "event-list export",
    )?;

    // A requested file is validated and counted as a stream. This preserves
    // the complete export without retaining every record in memory.
    if let Some(output_path) = user_output {
        let summary = inspect_event_csv_file_async(csv_path.to_path_buf())
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "Failed to generate a valid event list: {}\nstdout: {}\nstderr: {}",
                    error,
                    public_process_output(&stdout),
                    public_process_output(&stderr)
                )
            })?;
        persist_temp_path_async(csv_path, output_path.clone(), true).await?;
        let header = bounded_inline_header(summary.header);
        return Ok(EventListReport {
            success: true,
            output_path: Some(output_path.to_string_lossy().to_string()),
            message: Some(if header.is_some() {
                "Full event list saved to CSV".to_string()
            } else {
                "Full event list saved to CSV; oversized header omitted from the inline report"
                    .to_string()
            }),
            total_events: summary.total_events,
            offset: None,
            returned: None,
            truncated: None,
            next_offset: None,
            header,
            rows: None,
        });
    }

    // Inline responses remain bounded and materialize only up to the parser's
    // documented safety limit before returning a page.
    let parsed = read_csv_file_async(csv_path.to_path_buf())
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "Failed to generate a valid event list: {}\nstdout: {}\nstderr: {}",
                error,
                public_process_output(&stdout),
                public_process_output(&stderr)
            )
        })?;
    let page = event_page(&parsed, args.offset.unwrap_or(0), limit)?;
    let returned = page.rows.len();
    let end = page.offset + returned;
    let truncated = end < page.total_events;

    Ok(EventListReport {
        success: true,
        output_path: None,
        message: if truncated {
            Some(format!(
                "Showing rows {}..{} of {}{}. Pass offset={} for the next page, or output_path to save the full CSV.",
                page.offset,
                end,
                page.total_events,
                if page.byte_limited {
                    " (inline byte limit reached)"
                } else {
                    ""
                },
                end
            ))
        } else {
            None
        },
        total_events: page.total_events,
        offset: Some(page.offset),
        returned: Some(returned),
        truncated: Some(truncated),
        next_offset: if truncated { Some(end) } else { None },
        header: Some(page.header),
        rows: Some(page.rows),
    })
}

// ---------------------------------------------------------------------------
// Screenshot (with inline image content)
// ---------------------------------------------------------------------------

/// Inputs for [`handle_pix_get_screenshot`].
pub struct ScreenshotRequest {
    pub capture_path: String,
    pub output_path: String,
    pub depth: bool,
    pub marker: Option<String>,
    pub global_id: Option<u64>,
    pub rtv_index: Option<u64>,
    pub embed_image: bool,
    pub max_dimension: u32,
    /// Whether an existing destination may be replaced. Direct screenshot
    /// requests opt in; derived workflow screenshots use no-clobber semantics.
    pub replace_existing: bool,
}

pub async fn handle_pix_get_screenshot(req: ScreenshotRequest) -> Result<ScreenshotResult> {
    let ScreenshotRequest {
        capture_path,
        output_path,
        depth,
        marker,
        global_id,
        rtv_index,
        embed_image,
        max_dimension,
        replace_existing,
    } = req;

    let capture_file = require_capture_file_async(capture_path.clone()).await?;
    validate_screenshot_options(
        depth,
        marker.as_deref(),
        global_id,
        rtv_index,
        embed_image,
        max_dimension,
    )?;
    if output_path.trim().is_empty() {
        return Err(anyhow::anyhow!("output_path must not be empty"));
    }

    let output_path = if output_path.to_lowercase().ends_with(".png") {
        output_path
    } else {
        format!("{}.png", output_path)
    };
    let output_file =
        security::validate_output_file(Path::new(&output_path), "Screenshot output path")?;
    let output_path = output_file.to_string_lossy().into_owned();
    let temp_output = fresh_temp_path_async(".png", Some(output_file.clone())).await?;

    let pixtool = PixTool::find()?;
    let mut cmd = Command::new(&pixtool);
    cmd.arg("open-capture").arg(&capture_file);

    // Two documented paths:
    //  - `save-screenshot <png>`: the screenshot recorded when the capture was
    //    taken (fast, no replay). This is the reliable default.
    //  - `save-resource <png> [--rtv=N] [--depth] [--global-id=ID] [--marker=name]`:
    //    replays the capture and saves a render target / depth buffer for a
    //    specific draw.
    let used_save_resource =
        depth || marker.is_some() || global_id.is_some() || rtv_index.is_some();
    if used_save_resource {
        cmd.arg("save-resource").arg(temp_output.as_os_str());
        if let Some(n) = rtv_index {
            cmd.arg(format!("--rtv={}", n));
        }
        if depth {
            cmd.arg("--depth");
        }
        if let Some(id) = global_id {
            cmd.arg(format!("--global-id={}", id));
        }
        if let Some(ref m) = marker {
            push_value_option(&mut cmd, "--marker", m)?;
        }
    } else {
        cmd.arg("save-screenshot").arg(temp_output.as_os_str());
    }

    let output = run_pixtool_command(cmd, ANALYSIS_TIMEOUT, "pixtool screenshot export").await?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    validate_artifact_command_output(
        output.status.success(),
        output.status.code(),
        &stdout,
        &stderr,
        "screenshot export",
    )?;
    let validation_path = temp_output.to_path_buf();
    let validation_result = tokio::task::spawn_blocking(move || decode_png_file(&validation_path))
        .await
        .context("Screenshot validation task failed")?;
    let (file_size_bytes, decoded_image) = match validation_result {
        Ok(result) => result,
        Err(validation_error) => {
            return Err(anyhow::anyhow!(
                "Failed to save a valid screenshot: {}\nstdout: {}\nstderr: {}",
                validation_error,
                public_process_output(&stdout),
                public_process_output(&stderr)
            ));
        }
    };
    persist_temp_path_async(temp_output, output_file.clone(), replace_existing).await?;

    let image_b64 = if embed_image {
        let thumbnail_result = tokio::task::spawn_blocking(move || {
            encode_thumbnail_image(decoded_image, max_dimension)
        })
        .await
        .context("Screenshot thumbnail task failed")?;
        match thumbnail_result {
            Ok(b64) => Some(b64),
            Err(e) => {
                tracing::warn!("Could not encode screenshot thumbnail: {}", e);
                None
            }
        }
    } else {
        None
    };

    Ok(ScreenshotResult {
        report: ScreenshotReport {
            success: true,
            output_path,
            file_size_bytes,
            message: if used_save_resource {
                "Resource saved (render target / depth via capture replay)".to_string()
            } else {
                "Screenshot saved (frame recorded at capture time)".to_string()
            },
            image_embedded: image_b64.is_some(),
        },
        image_b64,
    })
}

/// Load a PNG, downscale so the longest side is at most `max_dim`, and return
/// base64-encoded PNG bytes suitable for an MCP image content block.
#[cfg(test)]
fn encode_thumbnail(path: &Path, max_dim: u32) -> Result<String> {
    let (_, img) = decode_png_file(path)?;
    encode_thumbnail_image(img, max_dim)
}

fn encode_thumbnail_image(img: image::DynamicImage, max_dim: u32) -> Result<String> {
    let scaled = if img.width().max(img.height()) > max_dim {
        img.thumbnail(max_dim, max_dim)
    } else {
        img
    };
    let mut buf = std::io::Cursor::new(Vec::new());
    scaled.write_to(&mut buf, image::ImageFormat::Png)?;
    let bytes = buf.into_inner();
    if bytes.len() > MAX_EMBEDDED_SCREENSHOT_BYTES {
        return Err(anyhow::anyhow!(
            "Embedded screenshot is too large ({} bytes; maximum {}). Lower max_dimension or set embed_image=false.",
            bytes.len(),
            MAX_EMBEDDED_SCREENSHOT_BYTES
        ));
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

// ---------------------------------------------------------------------------
// Counters / debug-layer analysis
// ---------------------------------------------------------------------------

fn validate_artifact_command_output(
    process_success: bool,
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
    operation: &str,
) -> Result<()> {
    reject_incomplete_process_output(stdout, stderr, operation)?;
    check_developer_mode(stdout, stderr)?;

    if let Some(line) = stdout
        .lines()
        .chain(stderr.lines())
        .find(|line| line_reports_issue(line, "error") || line_indicates_command_failure(line))
    {
        return Err(anyhow::anyhow!(
            "pixtool {operation} reported a failure ({line}).\n{}",
            combined_process_output(stdout, stderr)
        ));
    }

    // PIX 2603.25 may return 0xFFFFFFFF (-1) after successful analysis. Other
    // non-zero statuses are not documented successes and must not be hidden by
    // a partially-written artifact.
    if !process_success && exit_code != Some(-1) {
        return Err(anyhow::anyhow!(
            "pixtool {operation} failed with exit code {:?}.\n{}",
            exit_code,
            combined_process_output(stdout, stderr)
        ));
    }
    Ok(())
}

fn reject_incomplete_process_output(stdout: &str, stderr: &str, operation: &str) -> Result<()> {
    if stdout
        .lines()
        .chain(stderr.lines())
        .any(|line| line.trim().starts_with(PROCESS_OUTPUT_DIAGNOSTIC_PREFIX))
    {
        return Err(anyhow::anyhow!(
            "pixtool {operation} output was truncated or could not be drained within the server's {} byte per-stream limit; refusing to report an incomplete result",
            1024 * 1024
        ));
    }
    Ok(())
}

fn line_indicates_command_failure(line: &str) -> bool {
    let line = line.trim().to_lowercase();
    (line.starts_with("error") && line_reports_issue(&line, "error"))
        || line.starts_with("fatal")
        || line.contains(" failed")
        || line.starts_with("failed")
        || line.contains("exception")
        || line.contains("e_pix_")
        || line.contains("cannot open")
        || line.contains("could not open")
}

fn line_reports_issue(line: &str, issue: &str) -> bool {
    let line = line.trim().to_lowercase();
    let plural = format!("{issue}s");
    if line.starts_with(&format!("{issue}:"))
        || line.starts_with(&format!("[{issue}]"))
        || line.contains(&format!(" {issue}:"))
        || line.contains(&format!("[{issue}]"))
    {
        return true;
    }
    if !line
        .split(|character: char| !character.is_alphanumeric())
        .any(|token| token == issue || token == plural)
    {
        return false;
    }
    if line.contains("no errors or warnings") || line.contains("no warnings or errors") {
        return false;
    }

    let negated = [
        format!("no {issue}"),
        format!("0 {issue}"),
        format!("zero {issue}"),
        format!("without {issue}"),
        format!("{issue} count: 0"),
        format!("{issue} count = 0"),
        format!("{issue}_count=0"),
        format!("{issue}_count: 0"),
        format!("{plural}: 0"),
        format!("{plural}=0"),
        format!("{plural} found: 0"),
    ];
    if negated.iter().any(|pattern| line.contains(pattern)) {
        return false;
    }

    let tokens: Vec<&str> = line
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();
    tokens.iter().enumerate().any(|(index, token)| {
        if *token != issue && *token != plural {
            return false;
        }
        let start = index.saturating_sub(3);
        tokens[start..index]
            .iter()
            .filter_map(|value| value.parse::<u64>().ok())
            .any(|count| count > 0)
            || tokens
                .get(index + 1..=index + 2)
                .into_iter()
                .flatten()
                .filter_map(|value| value.parse::<u64>().ok())
                .any(|count| count > 0)
    })
}

fn classify_analysis_output(stdout: &str, stderr: &str) -> (Vec<String>, Vec<String>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    for line in stdout.lines().chain(stderr.lines()) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line_reports_issue(line, "error") || line_indicates_command_failure(line) {
            errors.push(line.to_string());
        }
        if line_reports_issue(line, "warning") {
            warnings.push(line.to_string());
        }
    }

    errors.sort();
    errors.dedup();
    warnings.sort();
    warnings.dedup();
    (errors, warnings)
}

fn bounded_diagnostic_lines(lines: Vec<String>) -> Result<(usize, Vec<String>, bool)> {
    let total = lines.len();
    let mut retained = Vec::new();
    let mut used_bytes = 2usize;
    let mut content_truncated = false;
    for line in lines {
        let line = public_process_output(&line);
        let (line, line_truncated) = truncate_utf8(&line, MAX_COUNTER_NAME_BYTES);
        let encoded_bytes = serde_json::to_vec(&line)?.len().saturating_add(1);
        if used_bytes.saturating_add(encoded_bytes) > MAX_INLINE_DIAGNOSTICS_JSON_BYTES {
            break;
        }
        used_bytes += encoded_bytes;
        content_truncated |= line_truncated;
        retained.push(line);
    }
    let truncated = content_truncated || retained.len() < total;
    Ok((total, retained, truncated))
}

fn combined_process_output(stdout: &str, stderr: &str) -> String {
    let stdout = public_process_output(stdout);
    let stderr = public_process_output(stderr);
    match (stdout.trim().is_empty(), stderr.trim().is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout,
        (true, false) => format!("stderr:\n{stderr}"),
        (false, false) => format!("stdout:\n{stdout}\nstderr:\n{stderr}"),
    }
}

fn public_process_output(output: &str) -> String {
    crate::security::sanitize_process_output(output)
}

fn is_counter_output_line(line: &str) -> bool {
    let line = line.trim();
    if line.is_empty() {
        return false;
    }
    let lower = line.to_ascii_lowercase();
    !lower.starts_with("launching")
        && !lower.starts_with("connecting")
        && !lower.starts_with("usage:")
        && !lower.starts_with("pixtool ")
        && !line.starts_with(PROCESS_OUTPUT_DIAGNOSTIC_PREFIX)
        && lower != "available counters:"
        && lower != "counters:"
        && !line_indicates_command_failure(line)
}

pub async fn handle_pix_list_counters(args: ListCountersArgs) -> Result<CountersReport> {
    let capture_path = require_capture_file_async(args.capture_path.clone()).await?;
    if args.limit == Some(0) {
        return Err(anyhow::anyhow!("limit must be at least 1"));
    }

    let pixtool = PixTool::find()?;
    let mut command = Command::new(&pixtool);
    command
        .arg("open-capture")
        .arg(&capture_path)
        .arg("list-counters");
    let output = run_pixtool_command(command, ANALYSIS_TIMEOUT, "pixtool counter listing").await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    reject_incomplete_process_output(&stdout, &stderr, "counter listing")?;
    check_developer_mode(&stdout, &stderr)?;
    if !output.status.success() && output.status.code() != Some(-1) {
        return Err(anyhow::anyhow!(
            "pixtool failed to list counters with status {}.\nstdout: {}\nstderr: {}",
            output.status,
            public_process_output(&stdout),
            public_process_output(&stderr)
        ));
    }
    if stdout
        .lines()
        .chain(stderr.lines())
        .any(|line| line_reports_issue(line, "error") || line_indicates_command_failure(line))
    {
        return Err(anyhow::anyhow!(
            "pixtool failed to list counters.\nstdout: {}\nstderr: {}",
            public_process_output(&stdout),
            public_process_output(&stderr)
        ));
    }
    let all: Vec<String> = stdout
        .lines()
        .filter(|line| is_counter_output_line(line))
        .map(|line| line.trim().to_string())
        .collect();
    let total_count = all.len();
    if total_count == 0 {
        return Err(anyhow::anyhow!(
            "pixtool returned no counters (status: {}).\nstdout: {}\nstderr: {}",
            output.status,
            public_process_output(&stdout),
            public_process_output(&stderr)
        ));
    }

    let filtered: Vec<String> = match args.filter.as_deref() {
        Some(f) => {
            let needle = f.to_lowercase();
            all.into_iter()
                .filter(|c| c.to_lowercase().contains(&needle))
                .collect()
        }
        None => all,
    };

    let limit = args.limit.unwrap_or(200).min(COUNTERS_MAX_LIMIT);
    let filtered_count = filtered.len();
    let mut returned_list = Vec::new();
    let mut used_bytes = 2usize;
    let mut content_truncated = false;
    for counter in filtered.into_iter().take(limit) {
        let (counter, counter_truncated) = truncate_utf8(&counter, MAX_COUNTER_NAME_BYTES);
        let encoded_bytes = serde_json::to_vec(&counter)?.len().saturating_add(1);
        if used_bytes.saturating_add(encoded_bytes) > MAX_INLINE_COUNTERS_JSON_BYTES {
            break;
        }
        used_bytes += encoded_bytes;
        content_truncated |= counter_truncated;
        returned_list.push(counter);
    }
    let truncated = content_truncated || returned_list.len() < filtered_count;

    let report = CountersReport {
        // pixtool analysis verbs can exit non-zero even on success; a parsed,
        // non-empty counter list is the success signal.
        success: true,
        capture_path: capture_path.to_string_lossy().into_owned(),
        total_count,
        returned: returned_list.len(),
        truncated,
        counters: returned_list,
    };
    if serde_json::to_vec(&report)?.len() > MAX_INLINE_ANALYSIS_PAYLOAD_BYTES {
        return Err(anyhow::anyhow!(
            "Counter report exceeded its {} byte inline budget; use a narrower filter",
            MAX_INLINE_ANALYSIS_PAYLOAD_BYTES
        ));
    }
    Ok(report)
}

pub async fn handle_pix_run_analysis(args: CapturePathArgs) -> Result<RunAnalysisReport> {
    let capture_path = require_capture_file_async(args.capture_path.clone()).await?;

    let pixtool = PixTool::find()?;
    let mut command = Command::new(&pixtool);
    command
        .arg("open-capture")
        .arg(&capture_path)
        .arg("run-debug-layer");
    let output =
        run_pixtool_command(command, ANALYSIS_TIMEOUT, "pixtool debug-layer playback").await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    reject_incomplete_process_output(&stdout, &stderr, "debug-layer playback")?;
    check_developer_mode(&stdout, &stderr)?;
    // PIX 2603.25 is known to return 0xFFFFFFFF (-1) from analysis verbs even
    // when playback succeeds. Other non-zero codes have no documented success
    // meaning and must not be silently reported as a valid replay.
    let exit_code = output.status.code();
    if !output.status.success() && exit_code != Some(-1) {
        return Err(anyhow::anyhow!(
            "pixtool debug-layer playback failed with status {}.\nstdout: {}\nstderr: {}",
            output.status,
            public_process_output(&stdout),
            public_process_output(&stderr)
        ));
    }

    let (errors, warnings) = classify_analysis_output(&stdout, &stderr);
    let (error_count, errors, errors_truncated) = bounded_diagnostic_lines(errors)?;
    let (warning_count, warnings, warnings_truncated) = bounded_diagnostic_lines(warnings)?;
    let combined_output = combined_process_output(&stdout, &stderr);
    let (full_output, output_truncated) =
        truncate_utf8(&combined_output, MAX_INLINE_PROCESS_OUTPUT_BYTES);
    let details_truncated = errors_truncated || warnings_truncated || output_truncated;

    let mut report = RunAnalysisReport {
        // run-debug-layer "doesn't generate output but validates playback"; its
        // exit code is unreliable, so report based on detected issues instead.
        success: error_count == 0,
        capture_path: capture_path.to_string_lossy().into_owned(),
        analysis: AnalysisDetail {
            error_count,
            warning_count,
            errors,
            warnings,
        },
        process_exit_code: exit_code,
        note: format!(
            "pixtool run-debug-layer validates capture playback but does not export D3D12 debug-layer messages; counts include only diagnostics printed by the command itself{}",
            if details_truncated {
                "; some diagnostic text was omitted to stay within the inline response budget"
            } else {
                ""
            }
        ),
        full_output,
    };
    if serde_json::to_vec(&report)?.len() > MAX_INLINE_ANALYSIS_PAYLOAD_BYTES {
        report.full_output.clear();
        report.note.push_str(
            "; full process output was omitted to stay within the inline response budget",
        );
    }
    if serde_json::to_vec(&report)?.len() > MAX_INLINE_ANALYSIS_PAYLOAD_BYTES {
        return Err(anyhow::anyhow!(
            "Debug-layer report exceeded its {} byte inline budget",
            MAX_INLINE_ANALYSIS_PAYLOAD_BYTES
        ));
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// Frame insights (heuristic, AI-oriented)
// ---------------------------------------------------------------------------

pub async fn handle_pix_analyze_frame(args: AnalyzeFrameArgs) -> Result<FrameInsights> {
    analyze_frame_insights(&args.capture_path, args.include_counters.unwrap_or(true)).await
}

fn bounded_frame_columns(headers: &StringRecord) -> Result<(Vec<String>, bool)> {
    let mut columns = Vec::new();
    let mut used_bytes = 2usize;
    let mut truncated = false;
    for header in headers {
        let (header, field_truncated) = truncate_utf8(header.trim(), MAX_FRAME_TEXT_FIELD_BYTES);
        let encoded_bytes = serde_json::to_vec(&header)?.len().saturating_add(1);
        if used_bytes.saturating_add(encoded_bytes) > MAX_FRAME_COLUMNS_JSON_BYTES {
            truncated = true;
            break;
        }
        used_bytes += encoded_bytes;
        truncated |= field_truncated;
        columns.push(header);
    }
    Ok((columns, truncated))
}

fn bounded_record_summary(record: &StringRecord) -> (String, bool) {
    let mut output = String::new();
    for (index, field) in record.iter().enumerate() {
        let separator = if index == 0 { "" } else { " | " };
        if output.len().saturating_add(separator.len()) >= MAX_FRAME_TEXT_FIELD_BYTES {
            return (output, true);
        }
        output.push_str(separator);
        let remaining = MAX_FRAME_TEXT_FIELD_BYTES.saturating_sub(output.len());
        let (field, truncated) = truncate_utf8(field, remaining);
        output.push_str(&field);
        if truncated {
            return (output, true);
        }
    }
    (output, false)
}

fn fit_frame_insights_payload(mut report: FrameInsights) -> Result<FrameInsights> {
    let mut note_added = false;
    loop {
        if serde_json::to_vec(&report)?.len() <= MAX_INLINE_ANALYSIS_PAYLOAD_BYTES {
            return Ok(report);
        }
        if !note_added {
            report.notes.push(
                "Some event columns or top events were omitted to stay within the inline response budget."
                    .to_string(),
            );
            note_added = true;
        }
        if report.top_events.pop().is_some() {
            continue;
        }
        if report.event_columns.pop().is_some() {
            continue;
        }
        return Err(anyhow::anyhow!(
            "Frame-insights report exceeded its {} byte inline budget",
            MAX_INLINE_ANALYSIS_PAYLOAD_BYTES
        ));
    }
}

/// Run pixtool to extract the event list and compute heuristic frame insights.
/// Shared by `pix_analyze_frame` and `pix_capture_and_analyze`.
fn frame_insights_from_csv(capture_path: &str, parsed: ParsedCsv) -> Result<FrameInsights> {
    let headers = &parsed.headers;
    let (columns, columns_truncated) = bounded_frame_columns(headers)?;
    // Locate an event-name column and a GPU-timing column heuristically.
    let name_col = ["event name", "name"]
        .iter()
        .find_map(|expected| {
            headers
                .iter()
                .position(|column| column.trim().eq_ignore_ascii_case(expected))
        })
        .or_else(|| {
            headers.iter().position(|column| {
                let (column, _) = truncate_utf8(column, MAX_FRAME_TEXT_FIELD_BYTES);
                let column = column.to_ascii_lowercase();
                column.contains("name") && !column.contains("id")
            })
        });
    // Only label values as GPU time when the column explicitly says GPU. A
    // generic/CPU "Duration" column is not a safe fallback.
    let time_col = ["gpu duration", "gpu time"]
        .iter()
        .find_map(|expected| {
            headers
                .iter()
                .position(|column| column.trim().eq_ignore_ascii_case(expected))
        })
        .or_else(|| {
            headers.iter().position(|column| {
                let (column, _) = truncate_utf8(column, MAX_FRAME_TEXT_FIELD_BYTES);
                let column = column.to_ascii_lowercase();
                column.contains("gpu") && (column.contains("time") || column.contains("duration"))
            })
        });

    let mut total = 0usize;
    let mut draws = 0usize;
    let mut dispatches = 0usize;
    let mut barriers = 0usize;
    let mut rt_changes = 0usize;
    let mut copies = 0usize;
    let mut timed: Vec<(String, f64)> = Vec::new();
    let mut event_names_truncated = false;

    for record in parsed.records {
        if record.is_empty() || record.iter().all(|value| value.trim().is_empty()) {
            continue;
        }
        total += 1;
        let (name, name_was_truncated) = match name_col.and_then(|index| record.get(index)) {
            Some(name) => truncate_utf8(name.trim(), MAX_FRAME_TEXT_FIELD_BYTES),
            None => bounded_record_summary(&record),
        };
        event_names_truncated |= name_was_truncated;
        let hay = name.to_ascii_lowercase();

        if hay.contains("draw") {
            draws += 1;
        }
        if hay.contains("dispatch") {
            dispatches += 1;
        }
        if hay.contains("barrier") {
            barriers += 1;
        }
        if hay.contains("setrendertarget") || hay.contains("omsetrendertargets") {
            rt_changes += 1;
        }
        if hay.contains("copy") {
            copies += 1;
        }

        if let Some(ti) = time_col
            && let Some(v) = record.get(ti)
            && let Ok(t) = v.trim().parse::<f64>()
            && t.is_finite()
        {
            timed.push((name.clone(), t));
            timed.sort_by(|a, b| b.1.total_cmp(&a.1));
            timed.truncate(10);
        }
    }

    let top_events: Vec<Value> = timed
        .iter()
        .map(|(name, t)| json!({ "event": name, "gpu_time": t }))
        .collect();

    let mut notes = vec![
        "Counts are heuristic, derived from event-name matching in the pixtool event list."
            .to_string(),
    ];
    if name_col.is_none() {
        notes.push(
            "No event-name column detected; counts were matched against full rows and may be approximate."
                .to_string(),
        );
    }
    if time_col.is_none() {
        notes.push(
            "No GPU-timing column was present in the exported event list, so expensive events could not be ranked."
                .to_string(),
        );
    }
    if columns_truncated {
        notes.push(
            "Event column labels were truncated to keep the response within its inline budget."
                .to_string(),
        );
    }
    if event_names_truncated {
        notes.push(
            "Exceptionally long event names were truncated in the heuristic summary.".to_string(),
        );
    }

    fit_frame_insights_payload(FrameInsights {
        success: true,
        capture_path: capture_path.to_string(),
        total_events: total,
        draw_calls: draws,
        dispatches,
        resource_barriers: barriers,
        render_target_changes: rt_changes,
        copies,
        event_columns: columns,
        top_events,
        notes,
    })
}

pub async fn analyze_frame_insights(
    capture_path: &str,
    include_counters: bool,
) -> Result<FrameInsights> {
    let capture_path = require_capture_file_async(capture_path.to_string()).await?;

    let csv = fresh_temp_path_async(".csv", None).await?;
    let pixtool = PixTool::find()?;
    let mut cmd = Command::new(&pixtool);
    cmd.arg("open-capture").arg(&capture_path);
    cmd.arg("save-event-list").arg(csv.as_os_str());
    if include_counters {
        push_value_option(&mut cmd, "--counters", "*")?;
    }
    let output = run_pixtool_command(cmd, ANALYSIS_TIMEOUT, "pixtool frame analysis").await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    validate_artifact_command_output(
        output.status.success(),
        output.status.code(),
        &stdout,
        &stderr,
        "frame analysis",
    )?;
    let parsed = match read_csv_file_async(csv.to_path_buf()).await {
        Ok(parsed) => parsed,
        Err(error) => {
            return Err(anyhow::anyhow!(
                "pixtool did not produce a valid event list: {}\nstdout: {}\nstderr: {}",
                error,
                public_process_output(&stdout),
                public_process_output(&stderr)
            ));
        }
    };

    // The validated CSV is accepted after status and explicit diagnostics have
    // also been checked above.
    let capture_path = capture_path.to_string_lossy().into_owned();
    tokio::task::spawn_blocking(move || frame_insights_from_csv(&capture_path, parsed))
        .await
        .context("Frame-insights processing task failed")?
}

/// Standard "capture not found" error with actionable guidance (SEP-1303).
fn capture_not_found(path: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "Capture file not found: {}. Provide a valid .wpix path; use pix_list_captures to find captures.",
        path
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structural_comparison_detects_insertions_without_global_id_noise() {
        let baseline = parse_comparison_csv_bytes(
            concat!(
                "Queue ID,Name,Global ID,GPU Duration\n",
                "0,Begin,1,1\n",
                "0,Draw A,2,2\n",
                "0,Draw A,3,3\n",
                "0,End,4,4\n"
            )
            .as_bytes(),
            100,
        )
        .expect("baseline CSV");
        let comparison = parse_comparison_csv_bytes(
            concat!(
                "Queue ID,Name,Global ID,GPU Duration\n",
                "0,Begin,101,1\n",
                "0,Draw A,102,2\n",
                "0,Dispatch,103,5\n",
                "0,Draw A,104,3\n",
                "0,End,105,4\n"
            )
            .as_bytes(),
            100,
        )
        .expect("comparison CSV");

        let structure = build_event_structure_comparison(&baseline, &comparison, 20)
            .expect("structural comparison");
        assert_eq!(structure.scope, EventComparisonScope::FullCapture);
        assert_eq!(structure.total_event_difference, 1);
        assert_eq!(structure.sequence.common_prefix_events, 2);
        assert_eq!(structure.changed_event_kinds, 1);
        assert_eq!(structure.top_changes[0].event_name, "Dispatch");
        assert_eq!(structure.top_changes[0].difference, 1);

        let timing = compare_gpu_timing(&baseline, &comparison).expect("timing aggregate");
        assert_eq!(timing.baseline.sum, 10.0);
        assert_eq!(timing.comparison.sum, 15.0);
        assert_eq!(timing.sum_difference_percent.as_deref(), Some("50.00%"));
    }

    #[test]
    fn structural_comparison_reports_prefix_scope_when_event_bound_is_hit() {
        let csv = concat!(
            "Queue ID,Name,Global ID,GPU Duration\n",
            "0,Begin,1,1\n",
            "0,Draw,2,2\n",
            "0,End,3,3\n"
        );
        let baseline = parse_comparison_csv_bytes(csv.as_bytes(), 2).expect("bounded CSV");
        let comparison = parse_comparison_csv_bytes(csv.as_bytes(), 2).expect("bounded CSV");
        let structure = build_event_structure_comparison(&baseline, &comparison, 20)
            .expect("structural comparison");
        assert_eq!(structure.scope, EventComparisonScope::Prefix);
        assert!(structure.truncated);
        assert_eq!(structure.baseline_total_events, 3);
        assert_eq!(structure.baseline_analyzed_events, 2);
        assert_eq!(structure.sequence.common_suffix_events, None);
        assert!(compare_gpu_timing(&baseline, &comparison).is_none());
    }

    #[test]
    fn comparison_does_not_treat_gpu_timestamps_as_durations() {
        let csv = concat!(
            "Queue ID,Name,Global ID,GPU Start Time\n",
            "0,Begin,1,1000000\n",
            "0,End,2,2000000\n"
        );
        let baseline = parse_comparison_csv_bytes(csv.as_bytes(), 10).expect("baseline CSV");
        let comparison = parse_comparison_csv_bytes(csv.as_bytes(), 10).expect("comparison CSV");
        assert!(compare_gpu_timing(&baseline, &comparison).is_none());
    }

    #[test]
    fn comparison_file_parser_streams_and_handles_utf8_bom() {
        let directory = tempfile::tempdir().expect("comparison directory");
        let csv_path = directory.path().join("events.csv");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"Queue ID,Name,Global ID,GPU Duration (ns)\n0,Draw,1,42\n");
        std::fs::write(&csv_path, bytes).expect("write comparison CSV");

        let parsed = read_comparison_csv(&csv_path, 10).expect("stream comparison CSV");
        assert_eq!(parsed.total_events, 1);
        assert_eq!(parsed.events.len(), 1);
        assert_eq!(parsed.events[0].identity.name, "Draw");
        assert_eq!(parsed.events[0].gpu_duration, Some(42.0));
    }

    #[test]
    fn comparison_report_trims_pathological_fields_to_inline_budget() {
        let pathological_name = "\u{1}".repeat(MAX_COMPARE_EVENT_FIELD_BYTES);
        let top_changes = (0..COMPARE_MAX_CHANGES)
            .map(|index| EventCountChange {
                queue_id: "0".to_string(),
                event_name: format!("{pathological_name}{index}"),
                baseline_count: 0,
                comparison_count: 1,
                difference: 1,
            })
            .collect();
        let report = CompareReport {
            success: true,
            capture_a: CaptureSide {
                path: "a.wpix".to_string(),
                size_bytes: 1,
                modified: None,
            },
            capture_b: CaptureSide {
                path: "b.wpix".to_string(),
                size_bytes: 1,
                modified: None,
            },
            comparison: metadata_comparison(1, 1),
            analysis_mode: CompareAnalysisMode::EventStructure,
            event_structure: Some(EventStructureComparison {
                scope: EventComparisonScope::FullCapture,
                baseline_total_events: 1,
                comparison_total_events: 1,
                total_event_difference: 0,
                baseline_analyzed_events: 1,
                comparison_analyzed_events: 1,
                truncated: false,
                sequence: EventSequenceComparison {
                    compared_positions: 1,
                    exact_position_matches: 1,
                    exact_position_match_percent: "100.00%".to_string(),
                    common_prefix_events: 1,
                    common_suffix_events: Some(0),
                },
                columns: EventColumnComparison {
                    added_count: 0,
                    removed_count: 0,
                    added: Vec::new(),
                    removed: Vec::new(),
                    truncated: false,
                },
                changed_event_kinds: COMPARE_MAX_CHANGES,
                changes_truncated: false,
                top_changes,
            }),
            gpu_timing: None,
            warnings: Vec::new(),
            suggestion: "Inspect changes".to_string(),
        };

        let fitted = fit_compare_report(report).expect("fit comparison report");
        assert!(serde_json::to_vec(&fitted).unwrap().len() <= MAX_COMPARE_REPORT_BYTES);
        let structure = fitted.event_structure.expect("event structure");
        assert!(structure.changes_truncated);
        assert!(structure.top_changes.len() < COMPARE_MAX_CHANGES);
    }

    #[test]
    fn test_encode_thumbnail_downscales_and_is_valid_png() {
        // A wide image that must be downscaled to fit within max_dim.
        let path = fresh_temp_path(".png", None).expect("reserve test png");
        let img = image::RgbImage::from_pixel(2000, 50, image::Rgb([200, 30, 30]));
        img.save(&path).expect("write test png");

        let b64 = encode_thumbnail(&path, 256).expect("encode thumbnail");
        assert!(!b64.is_empty());

        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .expect("valid base64");
        let decoded = image::load_from_memory(&bytes).expect("valid png");
        assert!(decoded.width() <= 256 && decoded.height() <= 256);
        assert!(validate_png_file(&path).expect("validate png") > 0);
    }

    #[test]
    fn test_response_format_default_limits() {
        assert_eq!(ResponseFormat::Summary.default_limit(), 50);
        assert_eq!(ResponseFormat::Full.default_limit(), 500);
    }

    #[test]
    fn test_csv_parser_handles_bom_quotes_commas_and_multiline_fields() {
        let csv = concat!(
            "\u{feff}\"Counter, Name\",Value,Note\r\n",
            "\"GPU \"\"Busy\"\"\",12.5,\"first line\nsecond line\"\r\n"
        );

        let parsed = parse_csv_bytes(csv.as_bytes()).expect("parse quoted CSV");
        assert_eq!(parsed.headers.get(0), Some("Counter, Name"));
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].get(0), Some("GPU \"Busy\""));
        assert_eq!(parsed.records[0].get(2), Some("first line\nsecond line"));

        let serialized = csv_record_to_string(&parsed.records[0]).expect("serialize record");
        assert!(serialized.contains("\"GPU \"\"Busy\"\"\""));
        assert!(serialized.contains("\"first line\nsecond line\""));
    }

    #[test]
    fn test_export_counters_maps_quoted_csv_fields() {
        let report = parse_csv_counters(
            "\u{feff}\"Counter, Name\",Value,Note\n\"GPU, Busy\",12.5,\"a,b\"\n",
        )
        .expect("parse counters");

        assert_eq!(report.row_count, Some(1));
        assert_eq!(
            report.columns,
            Some(vec![
                "Counter, Name".to_string(),
                "Value".to_string(),
                "Note".to_string()
            ])
        );
        assert_eq!(report.data[0]["Counter, Name"], json!("GPU, Busy"));
        assert_eq!(report.data[0]["Value"], json!(12.5));
        assert_eq!(report.data[0]["Note"], json!("a,b"));
    }

    #[test]
    fn test_export_counters_preserves_large_integer_precision() {
        let report = parse_csv_counters("Counter,Value\nSamples,9007199254740993\n")
            .expect("parse counters");

        assert_eq!(report.data[0]["Value"], json!(9_007_199_254_740_993_i64));
    }

    #[test]
    fn test_event_list_requires_pix_identity_headers() {
        for header in ["Name,Global ID", "Queue ID,Global ID", "Queue ID,Name"] {
            let parsed = parse_csv_bytes(format!("{header}\nvalue,1\n").as_bytes())
                .expect("parse syntactically valid CSV");
            assert!(
                validate_event_list_headers(&parsed.headers).is_err(),
                "unexpectedly accepted {header}"
            );
        }
    }

    #[test]
    fn test_frame_insights_use_logical_csv_records_and_valid_artifact_success() {
        let parsed = parse_csv_bytes(
            b"Name,GPU Duration\n\"Draw, Indexed\",1.25\nDispatch,4.5\nResourceBarrier,0.5\n",
        )
        .expect("parse frame CSV");
        let report = frame_insights_from_csv("capture.wpix", parsed).expect("frame insights");

        assert!(report.success);
        assert_eq!(report.total_events, 3);
        assert_eq!(report.draw_calls, 1);
        assert_eq!(report.dispatches, 1);
        assert_eq!(report.resource_barriers, 1);
        assert_eq!(report.top_events[0]["event"], json!("Dispatch"));
        assert_eq!(report.top_events[0]["gpu_time"], json!(4.5));
    }

    #[test]
    fn test_frame_insights_prefer_event_name_and_gpu_duration_columns() {
        let parsed = parse_csv_bytes(
            b"Event ID,Event Name,CPU Duration,GPU Duration\nDrawId,Dispatch,999,2.5\n",
        )
        .expect("parse frame CSV");
        let report = frame_insights_from_csv("capture.wpix", parsed).expect("frame insights");

        assert_eq!(report.dispatches, 1);
        assert_eq!(report.draw_calls, 0);
        assert_eq!(report.top_events[0]["event"], json!("Dispatch"));
        assert_eq!(report.top_events[0]["gpu_time"], json!(2.5));
    }

    #[test]
    fn test_event_limit_rejects_zero_and_caps_large_values() {
        assert!(event_list_limit(Some(0), ResponseFormat::Summary).is_err());
        assert_eq!(
            event_list_limit(None, ResponseFormat::Summary).expect("default"),
            50
        );
        assert_eq!(
            event_list_limit(Some(usize::MAX), ResponseFormat::Full).expect("capped"),
            EVENT_LIST_MAX_LIMIT
        );
    }

    #[test]
    fn test_event_page_enforces_inline_byte_budget() {
        let mut first = StringRecord::new();
        first.extend(["0", &"a".repeat(600_000), "1"]);
        let mut second = StringRecord::new();
        second.extend(["0", &"b".repeat(600_000), "2"]);
        let parsed = ParsedCsv {
            headers: StringRecord::from(vec!["Queue ID", "Name", "Global ID"]),
            records: vec![first, second],
        };

        let page = event_page(&parsed, 0, 2).expect("bounded page");
        assert_eq!(page.rows.len(), 1);
        assert!(page.byte_limited);
        assert_eq!(page.offset + page.rows.len(), 1);

        let mut oversized = StringRecord::new();
        oversized.extend(["0", &"x".repeat(MAX_INLINE_EVENT_PAYLOAD_BYTES), "1"]);
        let parsed = ParsedCsv {
            headers: StringRecord::from(vec!["Queue ID", "Name", "Global ID"]),
            records: vec![oversized],
        };
        assert!(event_page(&parsed, 0, 1).is_err());
        assert!(bounded_inline_header("h".repeat(MAX_INLINE_EVENT_PAYLOAD_BYTES)).is_none());
        assert_eq!(
            bounded_inline_header("Queue ID,Name,Global ID".to_string()).as_deref(),
            Some("Queue ID,Name,Global ID")
        );
    }

    #[test]
    fn test_analyze_preview_enforces_inline_byte_budget() {
        let mut oversized = StringRecord::new();
        oversized.extend(["0", &"x".repeat(MAX_ANALYZE_PREVIEW_ROW_BYTES + 1), "1"]);
        let parsed = ParsedCsv {
            headers: StringRecord::from(vec!["Queue ID", "Name", "Global ID"]),
            records: vec![oversized],
        };

        let summary = analyze_events_summary(&parsed).expect("bounded analysis preview");
        assert_eq!(summary["preview"].as_array().expect("preview").len(), 0);
        assert_eq!(summary["preview_truncated"], json!(true));
        assert!(
            serde_json::to_vec(&summary)
                .expect("serialize summary")
                .len()
                <= MAX_ANALYZE_EVENTS_JSON_BYTES
        );
    }

    #[test]
    fn test_frame_insights_bounds_pathological_event_names() {
        let mut record = StringRecord::new();
        record.extend([&"Draw".repeat(MAX_FRAME_TEXT_FIELD_BYTES), "1.25"]);
        let parsed = ParsedCsv {
            headers: StringRecord::from(vec!["Name", "GPU Duration"]),
            records: vec![record],
        };

        let report = frame_insights_from_csv("capture.wpix", parsed).expect("frame insights");
        assert!(
            report.notes.iter().any(|note| note.contains("truncated")),
            "the report should disclose event-name truncation"
        );
        assert!(
            serde_json::to_vec(&report).expect("serialize report").len()
                <= MAX_INLINE_ANALYSIS_PAYLOAD_BYTES
        );
    }

    #[test]
    fn test_fresh_temp_artifact_replaces_stale_destination_only_after_validation() {
        let directory = tempfile::tempdir().expect("test directory");
        let destination = directory.path().join("events.csv");
        std::fs::write(&destination, "old").expect("write stale destination");

        let temp = fresh_temp_path(".csv", Some(&destination)).expect("fresh temp path");
        assert!(!temp.exists());
        assert_ne!(temp.parent(), destination.parent());
        assert_eq!(
            std::fs::read_to_string(&destination).expect("read stale destination"),
            "old"
        );

        std::fs::write(&temp, "Queue ID,Name,Global ID\n0,Draw,1\n").expect("write fresh artifact");
        read_csv_file(&temp).expect("validate fresh artifact");
        persist_temp_path(temp, &destination, true).expect("replace destination");
        assert_eq!(
            std::fs::read_to_string(destination).expect("read replaced destination"),
            "Queue ID,Name,Global ID\n0,Draw,1\n"
        );
    }

    #[test]
    fn test_fresh_temp_artifact_no_clobber_preserves_existing_destination() {
        let directory = tempfile::tempdir().expect("test directory");
        let destination = directory.path().join("derived.png");
        std::fs::write(&destination, "existing").expect("write existing destination");

        let temp = fresh_temp_path(".png", Some(&destination)).expect("fresh temp path");
        std::fs::write(&temp, "new").expect("write staged output");
        assert!(persist_temp_path(temp, &destination, false).is_err());
        assert_eq!(
            std::fs::read_to_string(destination).expect("read destination"),
            "existing"
        );
    }

    #[test]
    fn test_screenshot_option_validation_rejects_invalid_combinations_and_ranges() {
        assert!(validate_screenshot_options(false, None, None, None, true, 0).is_err());
        assert!(
            validate_screenshot_options(
                false,
                None,
                None,
                None,
                true,
                SCREENSHOT_MAX_DIMENSION + 1,
            )
            .is_err()
        );
        assert!(validate_screenshot_options(true, None, None, Some(0), true, 1280).is_err());
        assert!(
            validate_screenshot_options(false, Some("marker"), Some(42), None, true, 1280).is_err()
        );
        assert!(validate_screenshot_options(false, Some("  "), None, None, true, 1280).is_err());
        assert!(validate_screenshot_options(false, Some("frame"), None, None, true, 1280).is_ok());
        assert!(validate_screenshot_options(false, None, None, Some(8), true, 1280).is_err());
        assert!(validate_screenshot_options(false, None, None, None, false, 0).is_ok());
    }

    #[test]
    fn test_analysis_classifier_ignores_zero_and_negated_counts() {
        let stdout = "0 errors\nNo warnings\nPlayback completed without errors";
        let stderr = "error count: 0\nwarning count: 0\nNo errors or warnings";
        let (errors, warnings) = classify_analysis_output(stdout, stderr);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    #[test]
    fn test_analysis_classifier_detects_real_issues_and_combines_stderr() {
        let stdout = "ERROR: invalid resource state\nwarning: expensive barrier";
        let stderr = "fatal playback failure";
        let (errors, warnings) = classify_analysis_output(stdout, stderr);
        assert_eq!(errors.len(), 2);
        assert_eq!(warnings, vec!["warning: expensive barrier"]);

        let combined = combined_process_output(stdout, stderr);
        assert!(combined.contains("stdout:"));
        assert!(combined.contains("stderr:\nfatal playback failure"));
    }

    #[test]
    fn test_analysis_classifier_uses_error_tokens_without_hiding_later_errors() {
        let (errors, warnings) =
            classify_analysis_output("ErrorScale,1.0\nNo errors; ERROR: device removed", "");
        assert_eq!(errors, vec!["No errors; ERROR: device removed"]);
        assert!(warnings.is_empty());
    }

    #[test]
    fn test_artifact_status_accepts_success_and_pix_minus_one_only() {
        assert!(validate_artifact_command_output(true, Some(0), "", "", "test").is_ok());
        assert!(validate_artifact_command_output(false, Some(-1), "", "", "test").is_ok());
        assert!(validate_artifact_command_output(false, Some(1), "", "", "test").is_err());
        assert!(
            validate_artifact_command_output(
                true,
                Some(0),
                PROCESS_OUTPUT_TRUNCATION_MARKER,
                "",
                "test"
            )
            .is_err()
        );
        assert!(!is_counter_output_line(PROCESS_OUTPUT_TRUNCATION_MARKER));
        let pipe_marker =
            "[pix-mcp: stdout pipe remained open after pixtool exited; output discarded]";
        assert!(reject_incomplete_process_output(pipe_marker, "", "test").is_err());
        assert!(!is_counter_output_line(pipe_marker));
        assert!(
            validate_artifact_command_output(
                true,
                Some(0),
                "No errors; ERROR: partial export",
                "",
                "test"
            )
            .is_err()
        );
    }

    #[test]
    fn test_event_output_rejects_capture_paths() {
        assert!(event_output_path(Some("capture.wpix")).is_err());
        assert!(event_output_path(Some("events.CSV")).is_ok());
        assert!(event_output_path(Some(" ")).is_err());
    }

    #[test]
    fn test_file_backed_event_validation_streams_past_inline_row_cap() {
        use std::io::Write;

        let artifact = fresh_temp_path(".csv", None).expect("event artifact");
        let file = std::fs::File::create(artifact.as_ref()).expect("create event CSV");
        let mut writer = std::io::BufWriter::new(file);
        writeln!(writer, "Queue ID,Name,Global ID").expect("write header");
        for index in 0..=MAX_ANALYSIS_CSV_ROWS {
            writeln!(writer, "0,Draw,{index}").expect("write row");
        }
        writer.flush().expect("flush CSV");

        let summary = inspect_event_csv_file(&artifact).expect("stream event CSV");
        assert_eq!(summary.total_events, MAX_ANALYSIS_CSV_ROWS + 1);
    }

    #[test]
    fn test_failure_line_detection_distinguishes_counter_names_from_errors() {
        assert!(line_indicates_command_failure(
            "ERROR: capture could not be opened"
        ));
        assert!(line_indicates_command_failure("Playback failed"));
        assert!(!line_indicates_command_failure("D3D12 GPU Time"));
    }
}
