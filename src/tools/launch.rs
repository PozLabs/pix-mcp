//! Launch tool logic: start executables with PIX attached.

use std::path::PathBuf;

use anyhow::Result;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::pix::PixTool;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LaunchArgs {
    /// Path to the executable to launch.
    pub exe_path: String,
    /// Command line arguments to pass to the executable.
    #[serde(default)]
    pub args: Option<Vec<String>>,
    /// Working directory for the executable.
    #[serde(default)]
    pub working_dir: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LaunchAndCaptureArgs {
    /// Path to the executable to launch.
    pub exe_path: String,
    /// Command line arguments to pass to the executable.
    #[serde(default)]
    pub args: Option<Vec<String>>,
    /// Path to save the capture file (.wpix). If omitted, open the capture in PIX.
    #[serde(default)]
    pub capture_file: Option<String>,
    /// Working directory for the executable.
    #[serde(default)]
    pub working_dir: Option<String>,
}

/// Result of a launch operation.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct LaunchReport {
    pub success: bool,
    /// Process ID of the pixtool launcher, not the target application. For a
    /// completed capture this launcher may already have exited.
    pub process_id: u32,
    /// Human-readable status message.
    pub message: String,
}

pub async fn handle_pix_launch(args: LaunchArgs) -> Result<LaunchReport> {
    let exe_path = PathBuf::from(&args.exe_path);
    let cmd_args = args.args.unwrap_or_default();
    let cmd_args_ref: Vec<&str> = cmd_args.iter().map(|s| s.as_str()).collect();
    let working_dir = args.working_dir.map(PathBuf::from);

    let result = PixTool::launch(&exe_path, &cmd_args_ref, working_dir.as_deref()).await?;

    Ok(LaunchReport {
        success: true,
        process_id: result.process_id,
        message: result.message,
    })
}

pub async fn handle_pix_launch_and_capture(args: LaunchAndCaptureArgs) -> Result<LaunchReport> {
    let exe_path = PathBuf::from(&args.exe_path);
    let cmd_args = args.args.unwrap_or_default();
    let cmd_args_ref: Vec<&str> = cmd_args.iter().map(|s| s.as_str()).collect();
    let capture_file = match args.capture_file {
        Some(path) => Some(super::capture::normalize_wpix_output(
            &path,
            "capture_file",
        )?),
        None => None,
    };
    let working_dir = args.working_dir.map(PathBuf::from);

    let result = PixTool::launch_and_capture(
        &exe_path,
        &cmd_args_ref,
        capture_file.as_deref(),
        working_dir.as_deref(),
    )
    .await?;

    Ok(LaunchReport {
        success: true,
        process_id: result.process_id,
        message: result.message,
    })
}
