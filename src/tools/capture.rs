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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GpuCaptureArgs {
    /// Process ID (PID) of the running DX12 application to capture.
    pub process_id: u32,
    /// Path to save the capture file (.wpix extension).
    #[serde(default)]
    pub output_path: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
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
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TimingCaptureArgs {
    /// Process ID (PID) of the running application to capture.
    pub process_id: u32,
    /// Path to save the capture file (.wpix extension).
    #[serde(default)]
    pub output_path: Option<String>,
    /// Duration of the timing capture in seconds.
    #[serde(default)]
    pub duration_seconds: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListCapturesArgs {
    /// Directory to search for capture files (defaults to the current directory).
    #[serde(default)]
    pub directory: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
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
    pub count: usize,
    pub captures: Vec<CaptureEntry>,
}

/// Generic success message result.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MessageReport {
    pub success: bool,
    pub message: String,
}

/// Ensure the given path ends with a `.wpix` extension.
fn ensure_wpix(path: PathBuf) -> PathBuf {
    if path.extension().is_none_or(|e| e != "wpix") {
        path.with_extension("wpix")
    } else {
        path
    }
}

/// Resolve a (possibly elicited) output path string, returning an actionable
/// tool error if it is still missing.
fn require_output_path(output_path: Option<String>) -> Result<PathBuf> {
    let raw = output_path.ok_or_else(|| {
        anyhow::anyhow!("output_path is required: provide a path to save the .wpix capture")
    })?;
    Ok(ensure_wpix(PathBuf::from(raw)))
}

pub async fn handle_pix_gpu_capture(args: GpuCaptureArgs) -> Result<CaptureReport> {
    let output_path = require_output_path(args.output_path)?;
    let result = PixTool::gpu_capture_process(args.process_id, &output_path)?;

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
    )?;

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
        PixTool::timing_capture_process(args.process_id, &output_path, args.duration_seconds)?;

    Ok(CaptureReport {
        success: true,
        message: result.message,
        output_path: result.output_path.to_string_lossy().to_string(),
        pixtool_output: result.stdout,
        note: Some("Timing captures require administrator privileges".to_string()),
    })
}

pub async fn handle_pix_list_captures(args: ListCapturesArgs) -> Result<CaptureListReport> {
    let directory = args
        .directory
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let captures = PixTool::list_captures(&directory)?;
    let entries: Vec<CaptureEntry> = captures
        .iter()
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

    Ok(CaptureListReport {
        success: true,
        directory: directory.to_string_lossy().to_string(),
        count: entries.len(),
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

    PixTool::open_capture(&capture_path)?;

    Ok(MessageReport {
        success: true,
        message: format!("Opened capture in PIX: {}", capture_path.display()),
    })
}
