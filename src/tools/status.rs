//! Status and health-check logic for the PIX MCP server.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use rmcp::schemars;
use serde::Serialize;
use tokio::process::Command;

use crate::pix::PixTool;
use crate::pix::pixtool::{PROCESS_OUTPUT_DIAGNOSTIC_PREFIX, run_pixtool_command};
use crate::security;

const PIXTOOL_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Availability of a PIX component (e.g. pixtool.exe).
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct PixComponent {
    /// Whether the component was found.
    pub found: bool,
    /// Resolved path to the component, if found.
    pub path: Option<String>,
    /// Error encountered while locating the component, if any.
    pub error: Option<String>,
}

/// Result of `pix_status`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct StatusReport {
    pub success: bool,
    /// Human-readable summary of readiness.
    pub status: String,
    /// Whether the server is ready to take captures (pixtool.exe found).
    pub ready: bool,
    /// pixtool.exe availability.
    pub pixtool: PixComponent,
    /// Whether the process is running elevated (required for timing captures).
    pub is_admin: bool,
    /// Whether user-controlled application launches are allowed while elevated.
    pub elevated_launch_allowed: bool,
    /// Default directory used by capture listing and MCP resources.
    pub captures_directory: String,
    /// Note about the current privilege level.
    pub privileges_note: String,
    /// Actionable suggestions to fix any problems.
    pub suggestions: Vec<String>,
}

/// Report whether PIX (`pixtool.exe`) is available and whether the server is
/// running elevated (required for timing captures).
pub async fn handle_pix_status() -> Result<StatusReport> {
    let pixtool = match PixTool::find() {
        Ok(path) => {
            let probe_error = probe_pixtool(&path)
                .await
                .err()
                .map(|error| error.to_string());
            PixComponent {
                found: true,
                path: Some(path.to_string_lossy().to_string()),
                error: probe_error,
            }
        }
        Err(e) => PixComponent {
            found: false,
            path: None,
            error: Some(e.to_string()),
        },
    };

    let is_admin = security::is_elevated()?;
    let elevated_launch_allowed = security::elevated_launch_allowed()?;
    let captures_directory = security::capture_directory()?
        .to_string_lossy()
        .into_owned();
    let ready = pixtool.found && pixtool.error.is_none();

    let mut suggestions = Vec::new();
    if !pixtool.found {
        suggestions.push(
            "Install PIX from the Microsoft Store: \
             https://apps.microsoft.com/store/detail/pix-on-windows/9PGD9BTP9D71"
                .to_string(),
        );
        suggestions.push("Or set the PIXTOOL_PATH environment variable to pixtool.exe".to_string());
    } else if pixtool.error.is_some() {
        suggestions.push(
            "The resolved PIXTOOL_PATH could not be verified; point it to a working pixtool.exe"
                .to_string(),
        );
    }
    if !is_admin {
        suggestions.push("Run as administrator to enable timing captures".to_string());
    } else if !elevated_launch_allowed {
        suggestions.push(
            "Use a separate non-elevated server for application-launch tools; timing capture remains enabled here"
                .to_string(),
        );
    }
    if suggestions.is_empty() {
        suggestions.push("All systems operational".to_string());
    }

    Ok(StatusReport {
        success: true,
        status: if ready {
            "PIX is installed and ready".to_string()
        } else {
            "PIX is not fully configured".to_string()
        },
        ready,
        pixtool,
        is_admin,
        elevated_launch_allowed,
        captures_directory,
        privileges_note: if is_admin {
            if elevated_launch_allowed {
                "Timing captures and explicitly opted-in elevated application launches are enabled"
                    .to_string()
            } else {
                "Timing captures are enabled; application-launch tools are blocked while elevated"
                    .to_string()
            }
        } else {
            "GPU captures work; timing captures require admin".to_string()
        },
        suggestions,
    })
}

async fn probe_pixtool(path: &Path) -> Result<()> {
    let mut command = Command::new(path);
    command.args(["--log=off", "--output=quiet", "--help"]);
    let output =
        run_pixtool_command(command, PIXTOOL_PROBE_TIMEOUT, "pixtool health check").await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stdout
        .lines()
        .chain(stderr.lines())
        .any(|line| line.trim().starts_with(PROCESS_OUTPUT_DIAGNOSTIC_PREFIX))
    {
        return Err(anyhow::anyhow!(
            "pixtool health-check output exceeded the server limit"
        ));
    }
    let combined = format!("{stdout}\n{stderr}").to_ascii_lowercase();
    if !combined.contains("usage: pixtool") || !combined.contains("open-capture") {
        return Err(anyhow::anyhow!(
            "the resolved executable did not return recognizable pixtool help"
        ));
    }
    Ok(())
}
