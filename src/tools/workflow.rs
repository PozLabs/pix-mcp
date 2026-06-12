//! Consolidated workflow tool: launch + GPU capture + frame analysis.

use std::path::PathBuf;

use anyhow::Result;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::analysis::{analyze_frame_insights, FrameInsights};
use crate::pix::PixTool;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CaptureAndAnalyzeArgs {
    /// Path to the executable to launch and capture.
    pub exe_path: String,
    /// Command line arguments to pass to the executable.
    #[serde(default)]
    pub args: Option<Vec<String>>,
    /// Path to save the capture (.wpix). Resolved via elicitation if omitted.
    #[serde(default)]
    pub output_path: Option<String>,
    /// Working directory for the executable.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Also extract a screenshot of the final frame (default: true).
    #[serde(default)]
    pub include_screenshot: Option<bool>,
    /// Bound the capture to this many frames (e.g. 1) so pixtool finishes
    /// promptly and closes the launched app.
    #[serde(default)]
    pub frames: Option<u32>,
}

/// Combined result of launch + capture + analysis.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CaptureAndAnalyzeReport {
    pub success: bool,
    pub capture_path: String,
    pub capture_message: String,
    pub insights: FrameInsights,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot_path: Option<String>,
    /// Suggested follow-up tool calls.
    pub next_steps: Vec<String>,
}

pub async fn handle_pix_capture_and_analyze(
    args: CaptureAndAnalyzeArgs,
) -> Result<CaptureAndAnalyzeReport> {
    let exe = PathBuf::from(&args.exe_path);

    let raw_output = args.output_path.ok_or_else(|| {
        anyhow::anyhow!("output_path is required: provide a path to save the .wpix capture")
    })?;
    let output_path = {
        let p = PathBuf::from(&raw_output);
        if p.extension().is_none_or(|e| e != "wpix") {
            p.with_extension("wpix")
        } else {
            p
        }
    };

    let cmd_args = args.args.unwrap_or_default();
    let cmd_args_ref: Vec<&str> = cmd_args.iter().map(|s| s.as_str()).collect();
    let working_dir = args.working_dir.map(PathBuf::from);

    // 1. Launch + capture.
    let capture = PixTool::gpu_capture_launch(
        &exe,
        &cmd_args_ref,
        &output_path,
        working_dir.as_deref(),
        args.frames,
    )?;
    let capture_path = capture.output_path.to_string_lossy().to_string();

    // 2. Heuristic frame analysis (with counters for timing).
    let insights = analyze_frame_insights(&capture_path, true)?;

    // 3. Optional screenshot of the final frame (saved, not embedded).
    let screenshot_path = if args.include_screenshot.unwrap_or(true) {
        let png = output_path
            .with_extension("png")
            .to_string_lossy()
            .to_string();
        match super::analysis::handle_pix_get_screenshot(
            capture_path.clone(),
            png,
            None,
            None,
            false,
            1280,
        )
        .await
        {
            Ok(res) => Some(res.report.output_path),
            Err(e) => {
                tracing::warn!("screenshot during capture_and_analyze failed: {}", e);
                None
            }
        }
    } else {
        None
    };

    let next_steps = vec![
        format!("Open in PIX GUI: pix_open_capture {{ capture_path: \"{capture_path}\" }}"),
        format!("Full event list: pix_get_event_list {{ capture_path: \"{capture_path}\" }}"),
        format!("Debug-layer validation: pix_run_analysis {{ capture_path: \"{capture_path}\" }}"),
    ];

    Ok(CaptureAndAnalyzeReport {
        success: true,
        capture_path,
        capture_message: capture.message,
        insights,
        screenshot_path,
        next_steps,
    })
}
