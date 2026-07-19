//! Capture tool logic: GPU and timing captures via `pixtool.exe`.
//!
//! Captures are taken by `pixtool.exe`, which injects the PIX capturer into the
//! target process. The in-process `pix3.h` capture API is intentionally not
//! used, since it can only capture the process that loads it.
//!
//! `output_path` is optional on the argument types so that a missing value can
//! be resolved via elicitation (or returned as a tool error) rather than a
//! protocol error (SEP-1303). The tool layer resolves it before calling here.

use std::path::PathBuf;

use anyhow::Result;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::pix::PixTool;
use crate::security;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GpuCaptureArgs {
    /// Process ID (PID) of the running DX12 application to capture.
    #[schemars(range(min = 1))]
    pub process_id: u32,
    /// Path to save the capture file (.wpix extension).
    #[serde(default)]
    pub output_path: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GpuCaptureLaunchArgs {
    /// Path to the executable to launch.
    pub exe_path: String,
    /// Command line arguments to pass to the executable.
    #[serde(default)]
    pub args: Option<Vec<String>>,
    /// Path to save the capture file (.wpix extension).
    #[serde(default)]
    pub output_path: Option<String>,
    /// Working directory for the executable.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Bound the capture to this many frames (default 1, range 1..=120).
    #[serde(default)]
    #[schemars(range(min = 1, max = 120))]
    pub frames: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimingCaptureArgs {
    /// Process ID (PID) of the running application to capture.
    #[schemars(range(min = 1))]
    pub process_id: u32,
    /// Path to save the capture file (.wpix extension).
    #[serde(default)]
    pub output_path: Option<String>,
    /// Duration in milliseconds (default 100, range 1..=600000).
    #[serde(default)]
    #[schemars(range(min = 1, max = 600000))]
    pub duration_ms: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListCapturesArgs {
    /// Directory to search for capture files (defaults to the current directory).
    #[serde(default)]
    pub directory: Option<String>,
    /// Zero-based result offset (default 0).
    #[serde(default)]
    pub offset: Option<usize>,
    /// Maximum captures to return (default 100, maximum 500).
    #[serde(default)]
    #[schemars(range(min = 1, max = 500))]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenCaptureArgs {
    /// Path to the capture file to open in the PIX GUI.
    pub capture_path: String,
}

/// Result of a capture operation.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CaptureReport {
    pub success: bool,
    pub message: String,
    pub output_path: String,
    pub pixtool_output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A single capture file entry.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CaptureEntry {
    pub path: String,
    pub name: String,
    pub size_bytes: u64,
    /// Last-modified time as seconds since the Unix epoch.
    pub modified: Option<u64>,
}

/// Result of `pix_list_captures`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CaptureListReport {
    pub success: bool,
    pub directory: String,
    /// Total captures found before pagination.
    pub total_count: usize,
    pub offset: usize,
    pub returned: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    /// Number of captures in this response (kept for compatibility).
    pub count: usize,
    pub captures: Vec<CaptureEntry>,
}

/// Generic success message result.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MessageReport {
    pub success: bool,
    pub message: String,
}

/// Normalize an output with no extension, while rejecting directories and a
/// conflicting extension instead of silently writing to a different path.
pub(super) fn normalize_wpix_output(raw: &str, label: &str) -> Result<PathBuf> {
    if raw.trim().is_empty() {
        return Err(anyhow::anyhow!("{label} must not be empty"));
    }
    let path = PathBuf::from(raw);
    if path.is_dir() {
        return Err(anyhow::anyhow!(
            "{label} must name a file, not a directory: {}",
            path.display()
        ));
    }
    match path.extension().and_then(|extension| extension.to_str()) {
        None => Ok(path.with_extension("wpix")),
        Some(extension) if extension.eq_ignore_ascii_case("wpix") => Ok(path),
        Some(_) => Err(anyhow::anyhow!(
            "{label} must end with .wpix: {}",
            path.display()
        )),
    }
}

/// Resolve a (possibly elicited) output path string, returning an actionable
/// tool error if it is still missing.
fn require_output_path(output_path: Option<String>) -> Result<PathBuf> {
    let raw = output_path.ok_or_else(|| {
        anyhow::anyhow!("output_path is required: provide a path to save the .wpix capture")
    })?;
    normalize_wpix_output(&raw, "output_path")
}

pub async fn handle_pix_gpu_capture(args: GpuCaptureArgs) -> Result<CaptureReport> {
    let output_path = require_output_path(args.output_path)?;
    let result = PixTool::gpu_capture_process(args.process_id, &output_path).await?;

    Ok(CaptureReport {
        success: true,
        message: result.message,
        output_path: result.output_path.to_string_lossy().to_string(),
        pixtool_output: result.stdout,
        note: None,
    })
}

pub async fn handle_pix_gpu_capture_launch(args: GpuCaptureLaunchArgs) -> Result<CaptureReport> {
    let exe_path = PathBuf::from(&args.exe_path);
    let output_path = require_output_path(args.output_path)?;
    let cmd_args = args.args.unwrap_or_default();
    let cmd_args_ref: Vec<&str> = cmd_args.iter().map(|s| s.as_str()).collect();
    let working_dir = args.working_dir.map(PathBuf::from);

    let result = PixTool::gpu_capture_launch(
        &exe_path,
        &cmd_args_ref,
        &output_path,
        working_dir.as_deref(),
        args.frames,
    )
    .await?;

    Ok(CaptureReport {
        success: true,
        message: result.message,
        output_path: result.output_path.to_string_lossy().to_string(),
        pixtool_output: result.stdout,
        note: None,
    })
}

pub async fn handle_pix_timing_capture(args: TimingCaptureArgs) -> Result<CaptureReport> {
    let output_path = require_output_path(args.output_path)?;
    let result =
        PixTool::timing_capture_process(args.process_id, &output_path, args.duration_ms).await?;

    Ok(CaptureReport {
        success: true,
        message: result.message,
        output_path: result.output_path.to_string_lossy().to_string(),
        pixtool_output: result.stdout,
        note: Some("Timing captures require administrator privileges".to_string()),
    })
}

pub async fn handle_pix_list_captures(args: ListCapturesArgs) -> Result<CaptureListReport> {
    const MAX_LIMIT: usize = 500;
    let limit = args.limit.unwrap_or(100);
    if limit == 0 || limit > MAX_LIMIT {
        return Err(anyhow::anyhow!("limit must be between 1 and {MAX_LIMIT}"));
    }
    let offset = args.offset.unwrap_or(0);
    let directory = match args.directory {
        Some(directory) if directory.trim().is_empty() => {
            return Err(anyhow::anyhow!("directory must not be empty"));
        }
        Some(directory) => PathBuf::from(directory),
        None => security::capture_directory()?,
    };
    let directory = security::validate_input_directory(&directory, "Capture directory")?;

    let scan_directory = directory.clone();
    let captures = tokio::task::spawn_blocking(move || PixTool::list_captures(&scan_directory))
        .await
        .map_err(|error| anyhow::anyhow!("Capture directory scan task failed: {error}"))??;
    let total_count = captures.len();
    let offset = offset.min(total_count);
    let entries: Vec<CaptureEntry> = captures
        .iter()
        .skip(offset)
        .take(limit)
        .map(|c| CaptureEntry {
            path: c.path.to_string_lossy().to_string(),
            name: c.name.clone(),
            size_bytes: c.size_bytes,
            modified: c
                .modified
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
        })
        .collect();
    let returned = entries.len();
    let next = offset.saturating_add(returned);
    let truncated = next < total_count;

    Ok(CaptureListReport {
        success: true,
        directory: directory.to_string_lossy().to_string(),
        total_count,
        offset,
        returned,
        truncated,
        next_offset: truncated.then_some(next),
        count: returned,
        captures: entries,
    })
}

pub async fn handle_pix_open_capture(args: OpenCaptureArgs) -> Result<MessageReport> {
    let capture_path = PathBuf::from(&args.capture_path);

    if !capture_path.exists() {
        return Err(anyhow::anyhow!(
            "Capture file not found: {}. Use pix_list_captures to find captures.",
            capture_path.display()
        ));
    }

    PixTool::open_capture(&capture_path).await?;

    Ok(MessageReport {
        success: true,
        message: format!("Opened capture in PIX: {}", capture_path.display()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wpix_output_normalization_rejects_directories_and_conflicting_extensions() {
        let directory = tempfile::tempdir().expect("test directory");
        assert!(normalize_wpix_output(&directory.path().to_string_lossy(), "output_path").is_err());
        assert!(normalize_wpix_output("capture.png", "output_path").is_err());
        assert_eq!(
            normalize_wpix_output("capture", "output_path").expect("append extension"),
            PathBuf::from("capture.wpix")
        );
        assert_eq!(
            normalize_wpix_output("capture.WPIX", "output_path").expect("accept extension"),
            PathBuf::from("capture.WPIX")
        );
    }

    #[tokio::test]
    async fn capture_listing_is_paginated_and_reports_the_total() {
        let directory = tempfile::tempdir().expect("test directory");
        for name in ["one.wpix", "two.wpix", "three.wpix"] {
            std::fs::write(directory.path().join(name), b"capture").expect("write capture");
        }
        std::fs::write(directory.path().join("ignored.txt"), b"not a capture")
            .expect("write ignored file");

        let report = handle_pix_list_captures(ListCapturesArgs {
            directory: Some(directory.path().to_string_lossy().into_owned()),
            offset: Some(1),
            limit: Some(1),
        })
        .await
        .expect("list captures");

        assert_eq!(report.total_count, 3);
        assert_eq!(report.offset, 1);
        assert_eq!(report.returned, 1);
        assert_eq!(report.count, 1);
        assert!(report.truncated);
        assert_eq!(report.next_offset, Some(2));
        assert_eq!(report.captures.len(), 1);
    }
}
