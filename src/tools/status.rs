//! Status and health-check logic for the PIX MCP server.

use anyhow::Result;
use rmcp::schemars;
use serde::Serialize;

use crate::pix::PixTool;

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
    /// Note about the current privilege level.
    pub privileges_note: String,
    /// Actionable suggestions to fix any problems.
    pub suggestions: Vec<String>,
}

/// Report whether PIX (`pixtool.exe`) is available and whether the server is
/// running elevated (required for timing captures).
pub async fn handle_pix_status() -> Result<StatusReport> {
    let pixtool = match PixTool::find() {
        Ok(path) => PixComponent {
            found: true,
            path: Some(path.to_string_lossy().to_string()),
            error: None,
        },
        Err(e) => PixComponent {
            found: false,
            path: None,
            error: Some(e.to_string()),
        },
    };

    let is_admin = is_elevated();
    let ready = pixtool.found;

    let mut suggestions = Vec::new();
    if !pixtool.found {
        suggestions.push(
            "Install PIX from the Microsoft Store: \
             https://apps.microsoft.com/store/detail/pix-on-windows/9PGD9BTP9D71"
                .to_string(),
        );
        suggestions.push("Or set the PIXTOOL_PATH environment variable to pixtool.exe".to_string());
    }
    if !is_admin {
        suggestions.push("Run as administrator to enable timing captures".to_string());
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
        privileges_note: if is_admin {
            "Full access including timing captures".to_string()
        } else {
            "GPU captures work; timing captures require admin".to_string()
        },
        suggestions,
    })
}

/// Check if the current process is running with elevated privileges.
fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        use std::mem;
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::Security::{
            GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
        };
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        unsafe {
            let mut token_handle: HANDLE = HANDLE::default();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token_handle).is_err() {
                return false;
            }

            let mut elevation: TOKEN_ELEVATION = mem::zeroed();
            let mut size = mem::size_of::<TOKEN_ELEVATION>() as u32;

            let result = GetTokenInformation(
                token_handle,
                TokenElevation,
                Some(&mut elevation as *mut _ as *mut _),
                size,
                &mut size,
            );

            let _ = CloseHandle(token_handle);

            result.is_ok() && elevation.TokenIsElevated != 0
        }
    }

    #[cfg(not(windows))]
    {
        false
    }
}
