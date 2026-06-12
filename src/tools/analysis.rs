//! Analysis tool logic: parse captures, counters, comparisons, and frame
//! insights via `pixtool.exe`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use base64::Engine;
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::pix::pixtool::unique_temp_path;
use crate::pix::PixTool;

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

// ---------------------------------------------------------------------------
// Argument types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExportCountersArgs {
    /// Path to the PIX-exported counters file (CSV or JSON).
    pub file_path: String,
    /// File format: "csv", "json", or "auto" (detected from the extension).
    #[serde(default)]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CompareCapturesArgs {
    /// Path to the first capture file (baseline).
    pub capture_a: String,
    /// Path to the second capture file (comparison).
    pub capture_b: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AnalyzeCaptureArgs {
    /// Path to the .wpix capture file.
    pub capture_path: String,
    /// Include performance counters (slower but more detailed).
    #[serde(default)]
    pub include_counters: Option<bool>,
    /// Counter group pattern (e.g., "D3D*", "*GPU*").
    #[serde(default)]
    pub counter_pattern: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EventListArgs {
    /// Path to the .wpix capture file.
    pub capture_path: String,
    /// Path to save the full CSV output. When set, the file is written and a
    /// path is returned instead of inlining rows (token-efficient for big lists).
    #[serde(default)]
    pub output_path: Option<String>,
    /// Counter group pattern to include (e.g., "*", "D3D*").
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
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
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
    /// Save the resource at the end of a named PIX marker region instead of the
    /// recorded screenshot (uses `save-resource --marker=<name>`).
    #[serde(default)]
    pub marker: Option<String>,
    /// Embed the image inline so a vision model can see it (default: true).
    #[serde(default)]
    pub embed_image: Option<bool>,
    /// Max width/height for the inline thumbnail in pixels (default: 1280).
    #[serde(default)]
    pub max_dimension: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CapturePathArgs {
    /// Path to the .wpix capture file.
    pub capture_path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListCountersArgs {
    /// Path to the .wpix capture file.
    pub capture_path: String,
    /// Case-insensitive substring filter applied to counter names.
    #[serde(default)]
    pub filter: Option<String>,
    /// Max counters to return (default 200).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
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
pub struct CompareReport {
    pub success: bool,
    pub capture_a: CaptureSide,
    pub capture_b: CaptureSide,
    pub comparison: ComparisonDetail,
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
    let path = PathBuf::from(&args.file_path);
    if !path.exists() {
        return Err(anyhow::anyhow!("File not found: {}", args.file_path));
    }

    let format = args.format.as_deref().unwrap_or("auto");
    let detected_format = if format == "auto" {
        match path.extension().and_then(|e| e.to_str()) {
            Some("csv") => "csv",
            Some("json") => "json",
            _ => {
                return Err(anyhow::anyhow!(
                    "Cannot detect format from extension. Pass format: \"csv\" or \"json\"."
                ))
            }
        }
    } else {
        format
    };

    let content = std::fs::read_to_string(&path)?;
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
    let mut lines = content.lines();
    let header = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("Empty CSV file"))?;
    let columns: Vec<String> = header.split(',').map(|s| s.trim().to_string()).collect();

    let mut rows: Vec<Value> = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let values: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        let mut row = serde_json::Map::new();
        for (i, col) in columns.iter().enumerate() {
            if let Some(val) = values.get(i) {
                if let Ok(num) = val.parse::<f64>() {
                    row.insert(col.clone(), json!(num));
                } else {
                    row.insert(col.clone(), json!(val));
                }
            }
        }
        rows.push(Value::Object(row));
    }

    Ok(ExportCountersReport {
        success: true,
        format: "csv".to_string(),
        columns: Some(columns),
        row_count: Some(rows.len()),
        data: Value::Array(rows),
    })
}

fn parse_json_counters(content: &str) -> Result<ExportCountersReport> {
    let data: Value = serde_json::from_str(content)?;
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

pub async fn handle_pix_compare_captures(args: CompareCapturesArgs) -> Result<CompareReport> {
    let path_a = PathBuf::from(&args.capture_a);
    let path_b = PathBuf::from(&args.capture_b);

    if !path_a.exists() {
        return Err(anyhow::anyhow!("Capture A not found: {}", args.capture_a));
    }
    if !path_b.exists() {
        return Err(anyhow::anyhow!("Capture B not found: {}", args.capture_b));
    }

    let meta_a = std::fs::metadata(&path_a)?;
    let meta_b = std::fs::metadata(&path_b)?;

    let size_a = meta_a.len();
    let size_b = meta_b.len();
    let size_diff = size_b as i64 - size_a as i64;
    let size_diff_percent = if size_a > 0 {
        (size_diff as f64 / size_a as f64) * 100.0
    } else {
        0.0
    };

    let to_secs = |t: std::time::SystemTime| {
        t.duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    };

    Ok(CompareReport {
        success: true,
        capture_a: CaptureSide {
            path: args.capture_a,
            size_bytes: size_a,
            modified: meta_a.modified().ok().map(to_secs),
        },
        capture_b: CaptureSide {
            path: args.capture_b,
            size_bytes: size_b,
            modified: meta_b.modified().ok().map(to_secs),
        },
        comparison: ComparisonDetail {
            size_difference_bytes: size_diff,
            size_difference_percent: format!("{:.2}%", size_diff_percent),
            size_increased: size_diff > 0,
            note: if size_diff.abs() > (size_a as i64 / 10) {
                "Significant size difference detected - may indicate a regression".to_string()
            } else {
                "Size difference within normal range".to_string()
            },
        },
        suggestion:
            "For detailed comparison, export counters from both captures and use pix_export_counters"
                .to_string(),
    })
}

// ---------------------------------------------------------------------------
// Analyze (event preview)
// ---------------------------------------------------------------------------

pub async fn handle_pix_analyze_capture(args: AnalyzeCaptureArgs) -> Result<AnalyzeReport> {
    let path = PathBuf::from(&args.capture_path);
    if !path.exists() {
        return Err(capture_not_found(&args.capture_path));
    }

    let include_counters = args.include_counters.unwrap_or(false);
    let counter_pattern = args.counter_pattern.as_deref().unwrap_or("*");

    let event_csv = unique_temp_path("pix_events", "csv");

    let pixtool = PixTool::find()?;
    let mut cmd = std::process::Command::new(&pixtool);
    cmd.arg("open-capture").arg(&args.capture_path);
    cmd.arg("save-event-list").arg(&event_csv);
    if include_counters {
        cmd.arg(format!("--counter-groups={}", counter_pattern));
    }

    let output = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to run pixtool: {}", e))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let events = if event_csv.exists() {
        let content = std::fs::read_to_string(&event_csv)?;
        let lines: Vec<&str> = content.lines().collect();
        let event_count = lines.len().saturating_sub(1);
        let preview: Vec<Value> = lines
            .iter()
            .skip(1)
            .take(20)
            .map(|line| {
                let parts: Vec<&str> = line.split(',').collect();
                json!({ "raw": line, "fields": parts })
            })
            .collect();
        let header = lines.first().copied().unwrap_or("");
        let _ = std::fs::remove_file(&event_csv);
        json!({ "total_events": event_count, "header": header, "preview": preview })
    } else {
        json!({
            "error": "Failed to extract events",
            "stdout": stdout.to_string(),
            "stderr": stderr.to_string()
        })
    };

    let metadata = std::fs::metadata(&path)?;
    Ok(AnalyzeReport {
        success: output.status.success(),
        capture_path: args.capture_path,
        file_size_bytes: metadata.len(),
        file_size_mb: format!("{:.2}", metadata.len() as f64 / 1_048_576.0),
        events,
        pixtool_output: stdout.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Event list (paginated / token-efficient)
// ---------------------------------------------------------------------------

pub async fn handle_pix_get_event_list(args: EventListArgs) -> Result<EventListReport> {
    let path = PathBuf::from(&args.capture_path);
    if !path.exists() {
        return Err(capture_not_found(&args.capture_path));
    }

    let user_output = args.output_path.clone().map(PathBuf::from);
    let csv_path = user_output
        .clone()
        .unwrap_or_else(|| unique_temp_path("pix_event_list", "csv"));

    let pixtool = PixTool::find()?;
    let mut cmd = std::process::Command::new(&pixtool);
    cmd.arg("open-capture").arg(&args.capture_path);
    cmd.arg("save-event-list").arg(&csv_path);
    if let Some(pattern) = args.counters.as_deref() {
        cmd.arg(format!("--counter-groups={}", pattern));
    }

    let output = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to run pixtool: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(anyhow::anyhow!("pixtool failed:\n{}\n{}", stdout, stderr));
    }
    if !csv_path.exists() {
        return Err(anyhow::anyhow!("Failed to generate event list"));
    }

    let content = std::fs::read_to_string(&csv_path)?;
    let mut lines = content.lines();
    let header = lines.next().unwrap_or("").to_string();
    let data_rows: Vec<&str> = lines.collect();
    let total_events = data_rows.len();

    // When the caller asked for a file, just save it and report the count.
    if user_output.is_some() {
        return Ok(EventListReport {
            success: true,
            output_path: Some(csv_path.to_string_lossy().to_string()),
            message: Some("Full event list saved to CSV".to_string()),
            total_events,
            offset: None,
            returned: None,
            truncated: None,
            next_offset: None,
            header: Some(header),
            rows: None,
        });
    }

    // Otherwise return a paginated inline slice.
    let format = args.response_format.unwrap_or(ResponseFormat::Summary);
    let offset = args.offset.unwrap_or(0).min(total_events);
    let limit = args
        .limit
        .unwrap_or_else(|| format.default_limit())
        .min(EVENT_LIST_MAX_LIMIT);

    let slice: Vec<String> = data_rows
        .iter()
        .skip(offset)
        .take(limit)
        .map(|s| s.to_string())
        .collect();
    let returned = slice.len();
    let end = offset + returned;
    let truncated = end < total_events;

    let _ = std::fs::remove_file(&csv_path);

    Ok(EventListReport {
        success: true,
        output_path: None,
        message: if truncated {
            Some(format!(
                "Showing rows {}..{} of {}. Pass offset={} for the next page, or output_path to save the full CSV.",
                offset, end, total_events, end
            ))
        } else {
            None
        },
        total_events,
        offset: Some(offset),
        returned: Some(returned),
        truncated: Some(truncated),
        next_offset: if truncated { Some(end) } else { None },
        header: Some(header),
        rows: Some(slice),
    })
}

// ---------------------------------------------------------------------------
// Screenshot (with inline image content)
// ---------------------------------------------------------------------------

pub async fn handle_pix_get_screenshot(
    capture_path: String,
    output_path: String,
    depth: bool,
    marker: Option<String>,
    embed_image: bool,
    max_dimension: u32,
) -> Result<ScreenshotResult> {
    let path = PathBuf::from(&capture_path);
    if !path.exists() {
        return Err(capture_not_found(&capture_path));
    }

    let output_path = if output_path.to_lowercase().ends_with(".png") {
        output_path
    } else {
        format!("{}.png", output_path)
    };

    let pixtool = PixTool::find()?;
    let mut cmd = std::process::Command::new(&pixtool);
    cmd.arg("open-capture").arg(&capture_path);

    // Two documented paths:
    //  - `save-screenshot <png>`: the screenshot recorded when the capture was
    //    taken (fast, no replay). This is the reliable default.
    //  - `save-resource <png> [--depth] [--marker=<name>]`: replays the capture
    //    and saves a render target / depth buffer.
    let used_save_resource = depth || marker.is_some();
    if used_save_resource {
        cmd.arg("save-resource").arg(&output_path);
        if depth {
            cmd.arg("--depth");
        }
        if let Some(ref m) = marker {
            cmd.arg(format!("--marker={}", m));
        }
    } else {
        cmd.arg("save-screenshot").arg(&output_path);
    }

    let output = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to run pixtool: {}", e))?;

    let output_file = PathBuf::from(&output_path);
    if !output_file.exists() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(anyhow::anyhow!(
            "Failed to save screenshot.\nstdout: {}\nstderr: {}",
            stdout,
            stderr
        ));
    }

    let metadata = std::fs::metadata(&output_file)?;
    let image_b64 = if embed_image {
        match encode_thumbnail(&output_file, max_dimension) {
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
            file_size_bytes: metadata.len(),
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
fn encode_thumbnail(path: &Path, max_dim: u32) -> Result<String> {
    let img = image::open(path)?;
    let scaled = if img.width().max(img.height()) > max_dim {
        img.thumbnail(max_dim, max_dim)
    } else {
        img
    };
    let mut buf = std::io::Cursor::new(Vec::new());
    scaled.write_to(&mut buf, image::ImageFormat::Png)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(buf.into_inner()))
}

// ---------------------------------------------------------------------------
// Counters / debug-layer analysis
// ---------------------------------------------------------------------------

pub async fn handle_pix_list_counters(args: ListCountersArgs) -> Result<CountersReport> {
    let path = PathBuf::from(&args.capture_path);
    if !path.exists() {
        return Err(capture_not_found(&args.capture_path));
    }

    let pixtool = PixTool::find()?;
    let output = std::process::Command::new(&pixtool)
        .arg("open-capture")
        .arg(&args.capture_path)
        .arg("list-counters")
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to run pixtool: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let all: Vec<String> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter(|line| !line.contains("Launching") && !line.contains("Connecting"))
        .map(|s| s.to_string())
        .collect();
    let total_count = all.len();

    let filtered: Vec<String> = match args.filter.as_deref() {
        Some(f) => {
            let needle = f.to_lowercase();
            all.into_iter()
                .filter(|c| c.to_lowercase().contains(&needle))
                .collect()
        }
        None => all,
    };

    let limit = args.limit.unwrap_or(200);
    let returned_list: Vec<String> = filtered.into_iter().take(limit).collect();

    Ok(CountersReport {
        success: output.status.success(),
        capture_path: args.capture_path,
        total_count,
        returned: returned_list.len(),
        counters: returned_list,
    })
}

pub async fn handle_pix_run_analysis(args: CapturePathArgs) -> Result<RunAnalysisReport> {
    let path = PathBuf::from(&args.capture_path);
    if !path.exists() {
        return Err(capture_not_found(&args.capture_path));
    }

    let pixtool = PixTool::find()?;
    let output = std::process::Command::new(&pixtool)
        .arg("open-capture")
        .arg(&args.capture_path)
        .arg("run-debug-layer")
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to run pixtool: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let errors: Vec<String> = stdout
        .lines()
        .chain(stderr.lines())
        .filter(|line| line.to_lowercase().contains("error"))
        .map(|s| s.to_string())
        .collect();
    let warnings: Vec<String> = stdout
        .lines()
        .chain(stderr.lines())
        .filter(|line| line.to_lowercase().contains("warning"))
        .map(|s| s.to_string())
        .collect();

    Ok(RunAnalysisReport {
        success: output.status.success(),
        capture_path: args.capture_path,
        analysis: AnalysisDetail {
            error_count: errors.len(),
            warning_count: warnings.len(),
            errors,
            warnings,
        },
        full_output: stdout.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Frame insights (heuristic, AI-oriented)
// ---------------------------------------------------------------------------

pub async fn handle_pix_analyze_frame(args: AnalyzeFrameArgs) -> Result<FrameInsights> {
    analyze_frame_insights(&args.capture_path, args.include_counters.unwrap_or(true))
}

/// Run pixtool to extract the event list and compute heuristic frame insights.
/// Shared by `pix_analyze_frame` and `pix_capture_and_analyze`.
pub fn analyze_frame_insights(capture_path: &str, include_counters: bool) -> Result<FrameInsights> {
    let path = PathBuf::from(capture_path);
    if !path.exists() {
        return Err(capture_not_found(capture_path));
    }

    let csv = unique_temp_path("pix_frame", "csv");
    let pixtool = PixTool::find()?;
    let mut cmd = std::process::Command::new(&pixtool);
    cmd.arg("open-capture").arg(capture_path);
    cmd.arg("save-event-list").arg(&csv);
    if include_counters {
        cmd.arg("--counter-groups=*");
    }
    let output = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to run pixtool: {}", e))?;

    if !csv.exists() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!(
            "pixtool did not produce an event list: {}",
            stderr
        ));
    }

    let content = std::fs::read_to_string(&csv)?;
    let _ = std::fs::remove_file(&csv);

    let mut lines = content.lines();
    let header_line = lines.next().unwrap_or("");
    let columns: Vec<String> = header_line
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();

    // Locate an event-name column and a GPU-timing column heuristically.
    let name_col = columns.iter().position(|c| {
        let l = c.to_lowercase();
        l.contains("name") || l.contains("event")
    });
    let time_col = columns.iter().position(|c| {
        let l = c.to_lowercase();
        (l.contains("gpu") && (l.contains("time") || l.contains("duration")))
            || l.contains("duration")
    });

    let mut total = 0usize;
    let mut draws = 0usize;
    let mut dispatches = 0usize;
    let mut barriers = 0usize;
    let mut rt_changes = 0usize;
    let mut copies = 0usize;
    let mut timed: Vec<(String, f64)> = Vec::new();

    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        total += 1;
        let fields: Vec<&str> = line.split(',').collect();
        let name = name_col
            .and_then(|i| fields.get(i))
            .map(|s| s.trim())
            .unwrap_or(line);
        let hay = name.to_lowercase();

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

        if let Some(ti) = time_col {
            if let Some(v) = fields.get(ti) {
                if let Ok(t) = v.trim().parse::<f64>() {
                    timed.push((name.to_string(), t));
                }
            }
        }
    }

    timed.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let top_events: Vec<Value> = timed
        .iter()
        .take(10)
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
            "No GPU-timing column found; pass include_counters=true to rank the most expensive events."
                .to_string(),
        );
    }

    Ok(FrameInsights {
        success: output.status.success(),
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
    fn test_encode_thumbnail_downscales_and_is_valid_png() {
        // A wide image that must be downscaled to fit within max_dim.
        let path =
            std::env::temp_dir().join(format!("pixmcp_thumb_test_{}.png", std::process::id()));
        let img = image::RgbImage::from_pixel(2000, 50, image::Rgb([200, 30, 30]));
        img.save(&path).expect("write test png");

        let b64 = encode_thumbnail(&path, 256).expect("encode thumbnail");
        assert!(!b64.is_empty());

        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .expect("valid base64");
        let decoded = image::load_from_memory(&bytes).expect("valid png");
        assert!(decoded.width() <= 256 && decoded.height() <= 256);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_response_format_default_limits() {
        assert_eq!(ResponseFormat::Summary.default_limit(), 50);
        assert_eq!(ResponseFormat::Full.default_limit(), 500);
    }
}
