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

/// Capabilities detected from the installed binary's own top-level help.
/// `None` at the report level means probing failed; booleans here therefore
/// mean "advertised by this binary", not a guessed version threshold.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct PixCapabilities {
    pub gpu_capture: bool,
    pub programmatic_capture: bool,
    pub timing_capture: bool,
    pub event_analysis: bool,
    pub resource_export: bool,
    pub high_frequency_counters: bool,
    pub occupancy_collection: bool,
    pub capture_upgrade: bool,
    pub detected_commands: Vec<String>,
}

/// PixStorage is reported as an installation capability only. Loading an
/// installation-specific DLL and exposing arbitrary SQLite is intentionally
/// outside this server's trust model.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct PixStorageStatus {
    pub found: bool,
    pub path: Option<String>,
    pub query_api_enabled: bool,
    pub note: String,
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
    /// Version inferred from PIX's installation-directory name, when available.
    pub pixtool_version: Option<String>,
    /// Commands advertised by the installed pixtool binary.
    pub capabilities: Option<PixCapabilities>,
    /// Presence of the native timing-query library; never loaded by pix-mcp.
    pub pixstorage: PixStorageStatus,
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
    let mut pixtool_version = None;
    let mut capabilities = None;
    let mut pixstorage = unavailable_pixstorage();
    let pixtool = match PixTool::find() {
        Ok(path) => {
            pixtool_version = infer_installation_version(&path);
            pixstorage = inspect_pixstorage(&path);
            let probe_error = match probe_pixtool(&path).await {
                Ok(detected) => {
                    capabilities = Some(detected);
                    None
                }
                Err(error) => Some(error.to_string()),
            };
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
        pixtool_version,
        capabilities,
        pixstorage,
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

async fn probe_pixtool(path: &Path) -> Result<PixCapabilities> {
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
    let combined = format!("{stdout}\n{stderr}");
    if !combined.to_ascii_lowercase().contains("usage: pixtool")
        || !help_has_command(&combined, "open-capture")
    {
        return Err(anyhow::anyhow!(
            "the resolved executable did not return recognizable pixtool help"
        ));
    }
    Ok(parse_capabilities(&combined))
}

fn help_has_command(help: &str, expected: &str) -> bool {
    help.split(|character: char| !(character.is_ascii_alphanumeric() || character == '-'))
        .any(|token| token.eq_ignore_ascii_case(expected))
}

fn parse_capabilities(help: &str) -> PixCapabilities {
    const COMMANDS: &[&str] = &[
        "take-capture",
        "programmatic-capture",
        "take-new-timing-capture",
        "save-event-list",
        "save-resource",
        "save-screenshot",
        "save-high-frequency-counters",
        "collect-occupancy",
        "upgrade-gpu-capture",
    ];
    let detected_commands = COMMANDS
        .iter()
        .copied()
        .filter(|command| help_has_command(help, command))
        .map(str::to_string)
        .collect::<Vec<_>>();
    let has = |command: &str| detected_commands.iter().any(|item| item == command);
    PixCapabilities {
        gpu_capture: has("take-capture"),
        programmatic_capture: has("programmatic-capture"),
        timing_capture: has("take-new-timing-capture"),
        event_analysis: has("save-event-list"),
        resource_export: has("save-resource") && has("save-screenshot"),
        high_frequency_counters: has("save-high-frequency-counters"),
        occupancy_collection: has("collect-occupancy"),
        capture_upgrade: has("upgrade-gpu-capture"),
        detected_commands,
    }
}

fn infer_installation_version(pixtool: &Path) -> Option<String> {
    let candidate = pixtool.parent()?.file_name()?.to_str()?;
    let looks_like_version = candidate
        .chars()
        .any(|character| character.is_ascii_digit())
        && candidate
            .chars()
            .all(|character| character.is_ascii_digit() || character == '.');
    looks_like_version.then(|| candidate.to_string())
}

fn unavailable_pixstorage() -> PixStorageStatus {
    PixStorageStatus {
        found: false,
        path: None,
        query_api_enabled: false,
        note: "PixStorage.dll was not inspected because pixtool is unavailable. Native timing queries are not exposed."
            .to_string(),
    }
}

fn inspect_pixstorage(pixtool: &Path) -> PixStorageStatus {
    let candidate = pixtool.with_file_name("PixStorage.dll");
    let found = candidate.is_file();
    PixStorageStatus {
        found,
        path: found.then(|| candidate.to_string_lossy().to_string()),
        query_api_enabled: false,
        note: if found {
            "PixStorage.dll is installed, but pix-mcp does not load native DLLs or expose arbitrary SQLite. Use PIX to inspect Timing Captures."
                .to_string()
        } else {
            "PixStorage.dll was not found beside pixtool. Native timing queries are not exposed."
                .to_string()
        },
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_probe_uses_complete_command_tokens() {
        let help = concat!(
            "Usage: pixtool <command>\n",
            "take-capture programmatic-capture take-new-timing-capture\n",
            "save-event-list save-resource save-screenshot collect-occupancy\n",
            "save-high-frequency-counters upgrade-gpu-capture\n"
        );
        let capabilities = parse_capabilities(help);
        assert!(capabilities.gpu_capture);
        assert!(capabilities.programmatic_capture);
        assert!(capabilities.timing_capture);
        assert!(capabilities.event_analysis);
        assert!(capabilities.resource_export);
        assert!(capabilities.high_frequency_counters);
        assert!(capabilities.occupancy_collection);
        assert!(capabilities.capture_upgrade);

        // A longer token must not be treated as an advertised command.
        assert!(!help_has_command(
            "Usage: pixtool save-event-listing",
            "save-event-list"
        ));
    }

    #[test]
    fn version_is_only_inferred_from_version_like_installation_directories() {
        let versioned = std::path::PathBuf::from("root")
            .join("2603.25")
            .join("pixtool.exe");
        assert_eq!(
            infer_installation_version(&versioned),
            Some("2603.25".to_string())
        );
        let unversioned = std::path::PathBuf::from("root")
            .join("pix")
            .join("pixtool.exe");
        assert_eq!(infer_installation_version(&unversioned), None);
    }
}
