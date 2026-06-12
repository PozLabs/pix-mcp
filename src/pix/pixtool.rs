//! pixtool.exe wrapper for PIX operations
//!
//! This module provides subprocess execution of pixtool.exe for operations
//! that aren't available via the programmatic API.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use anyhow::{anyhow, Result};

static PIXTOOL_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Wrapper for pixtool.exe operations
pub struct PixTool;

/// Format a `--option="value"` pixtool argument with the value wrapped in
/// literal double quotes.
///
/// pixtool keeps a quoted value intact even when it contains spaces. Without
/// the quotes — or with the quotes around the whole `--option=value` token, as
/// `Command::arg` would produce on Windows — pixtool (verified on 2603.25)
/// re-splits the value on spaces and rejects everything after the first space
/// as an "Unknown option".
fn quoted_value_option(option: &str, value: &str) -> String {
    format!("{option}=\"{value}\"")
}

/// Append a `--option="value"` argument to `cmd`, preserving the literal quotes
/// around the value in the actual command line.
///
/// On Windows this needs `raw_arg`: `Command::arg` would quote the entire
/// `--option=value` token (`"--option=foo bar"`) instead of just the value
/// (`--option="foo bar"`), which pixtool mishandles.
#[cfg(windows)]
fn push_value_option(cmd: &mut Command, option: &str, value: &str) {
    use std::os::windows::process::CommandExt;
    cmd.raw_arg(quoted_value_option(option, value));
}

/// Non-Windows fallback (pixtool is Windows-only; this keeps the crate building
/// on other platforms for development and tests).
#[cfg(not(windows))]
fn push_value_option(cmd: &mut Command, option: &str, value: &str) {
    cmd.arg(format!("{option}={value}"));
}

impl PixTool {
    /// Find pixtool.exe in the PIX installation
    pub fn find() -> Result<PathBuf> {
        let mut guard = PIXTOOL_PATH
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;

        if let Some(ref path) = *guard {
            return Ok(path.clone());
        }

        // Check environment variable first
        if let Ok(path) = std::env::var("PIXTOOL_PATH") {
            let path = PathBuf::from(path);
            if path.exists() {
                *guard = Some(path.clone());
                return Ok(path);
            }
        }

        // Search in Program Files
        let program_files =
            std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".to_string());

        let pix_dir = PathBuf::from(&program_files).join("Microsoft PIX");

        if pix_dir.exists() {
            // Find the latest version
            let mut versions: Vec<_> = std::fs::read_dir(&pix_dir)?
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .collect();

            versions.sort_by_key(|e| std::cmp::Reverse(e.file_name()));

            if let Some(latest) = versions.first() {
                let tool_path = latest.path().join("pixtool.exe");
                if tool_path.exists() {
                    *guard = Some(tool_path.clone());
                    return Ok(tool_path);
                }
            }
        }

        Err(anyhow!(
            "Could not find pixtool.exe. \
             Install PIX from Microsoft Store or set PIXTOOL_PATH environment variable."
        ))
    }

    /// Launch an application with PIX attached for GPU capture
    pub fn launch(
        exe_path: &Path,
        args: &[&str],
        working_dir: Option<&Path>,
    ) -> Result<LaunchResult> {
        if !exe_path.exists() {
            return Err(anyhow!("Executable not found: {}", exe_path.display()));
        }
        let pixtool = Self::find()?;

        let mut cmd = Command::new(&pixtool);
        cmd.arg("launch");
        cmd.arg(exe_path);

        // Pass app args and working directory as quoted-value options *after*
        // the exe, matching the form pixtool accepts (quotes around the value).
        if !args.is_empty() {
            push_value_option(&mut cmd, "--command-line", &args.join(" "));
        }
        if let Some(dir) = working_dir {
            push_value_option(&mut cmd, "--working-directory", &dir.display().to_string());
        }

        // Use null for stdio to fully detach the process
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());
        cmd.stdin(Stdio::null());

        tracing::info!("Launching via pixtool: {:?}", exe_path);

        let child = cmd
            .spawn()
            .map_err(|e| anyhow!("Failed to launch pixtool: {}", e))?;

        let pid = child.id();

        // Spawn a thread to wait for the child to prevent zombie processes
        std::thread::spawn(move || {
            let _ = child.wait_with_output();
        });

        Ok(LaunchResult {
            process_id: pid,
            message: format!(
                "Launched {} under pixtool (pixtool PID: {} — this is the launcher process, not \
                 the game). For a programmatic GPU capture use pix_gpu_capture_launch or \
                 pix_capture_and_analyze: PIX can only GPU-capture a process it launched itself, \
                 so attaching by PID to a separately-started game will fail.",
                exe_path.display(),
                pid
            ),
        })
    }

    /// Launch an application with PIX and immediately start capturing
    pub fn launch_and_capture(
        exe_path: &Path,
        args: &[&str],
        capture_file: Option<&Path>,
        working_dir: Option<&Path>,
    ) -> Result<LaunchResult> {
        if !exe_path.exists() {
            return Err(anyhow!("Executable not found: {}", exe_path.display()));
        }
        let pixtool = Self::find()?;

        let mut cmd = Command::new(&pixtool);
        cmd.arg("launch");
        cmd.arg(exe_path);
        // Begin capturing as soon as the app starts rendering.
        cmd.arg("--captureFromStart");

        if !args.is_empty() {
            push_value_option(&mut cmd, "--command-line", &args.join(" "));
        }
        if let Some(dir) = working_dir {
            push_value_option(&mut cmd, "--working-directory", &dir.display().to_string());
        }

        // If a destination is provided, save the capture taken from start to it.
        if let Some(file) = capture_file {
            cmd.arg("save-capture");
            cmd.arg(file);
        }

        // Use null for stdio to fully detach the process
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());
        cmd.stdin(Stdio::null());

        tracing::info!("Launching with capture via pixtool: {:?}", exe_path);

        let child = cmd
            .spawn()
            .map_err(|e| anyhow!("Failed to launch pixtool: {}", e))?;

        let pid = child.id();

        // Spawn a thread to wait for the child to prevent zombie processes
        std::thread::spawn(move || {
            let _ = child.wait_with_output();
        });

        let message = match capture_file {
            Some(file) => format!(
                "Launched {} with PIX capturing from start (PID: {}); capture will be saved to {}",
                exe_path.display(),
                pid,
                file.display()
            ),
            None => format!(
                "Launched {} with PIX capturing from start (PID: {}); use PIX to save the capture",
                exe_path.display(),
                pid
            ),
        };

        Ok(LaunchResult {
            process_id: pid,
            message,
        })
    }

    /// Open a capture file in the PIX GUI
    pub fn open_capture(capture_path: &Path) -> Result<()> {
        // Find WinPix.exe (GUI) in same directory as pixtool
        let pixtool = Self::find()?;
        let pix_dir = pixtool
            .parent()
            .ok_or_else(|| anyhow!("Cannot find PIX directory"))?;
        let winpix = pix_dir.join("WinPix.exe");

        if !winpix.exists() {
            return Err(anyhow!("WinPix.exe not found at: {}", winpix.display()));
        }

        // spawn() returns Child, not status. Just return Ok since we spawned successfully
        let _child = Command::new(&winpix)
            .arg(capture_path)
            .spawn()
            .map_err(|e| anyhow!("Failed to launch WinPix: {}", e))?;

        Ok(())
    }

    /// Take a GPU capture of a running process
    /// Uses: pixtool attach <PID> take-capture save-capture <file.wpix>
    pub fn gpu_capture_process(process_id: u32, output_path: &Path) -> Result<CaptureResult> {
        let pixtool = Self::find()?;

        tracing::info!(
            "Starting GPU capture of PID {} to {:?}",
            process_id,
            output_path
        );

        let output = Command::new(&pixtool)
            .arg("attach")
            .arg(process_id.to_string())
            .arg("take-capture")
            .arg("save-capture")
            .arg(output_path)
            .output()
            .map_err(|e| anyhow!("Failed to run pixtool: {}", e))?;

        if output.status.success() {
            Ok(CaptureResult {
                output_path: output_path.to_path_buf(),
                message: format!("GPU capture saved to {}", output_path.display()),
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            })
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let combined = format!("{} {}", stdout, stderr).to_lowercase();
            if combined.contains("pixtool17") || combined.contains("not launched for gpu capture") {
                return Err(anyhow!(
                    "Cannot GPU-capture PID {}: the process was not launched under PIX \
                     (PIXTOOL17 - Process not launched for GPU Capture). PIX can only take a GPU \
                     capture of a process that PIX itself launched. Use pix_gpu_capture_launch or \
                     pix_capture_and_analyze to launch the app under PIX and capture in one step.\n\
                     stdout: {}\nstderr: {}",
                    process_id,
                    stdout,
                    stderr
                ));
            }
            Err(anyhow!(
                "pixtool capture failed:\nstderr: {}\nstdout: {}",
                stderr,
                stdout
            ))
        }
    }

    /// Launch executable and capture, then save
    /// Uses: pixtool launch <exe> take-capture save-capture <file.wpix>
    pub fn gpu_capture_launch(
        exe_path: &Path,
        args: &[&str],
        output_path: &Path,
        working_dir: Option<&Path>,
        frames: Option<u32>,
    ) -> Result<CaptureResult> {
        if !exe_path.exists() {
            return Err(anyhow!("Executable not found: {}", exe_path.display()));
        }
        let pixtool = Self::find()?;

        tracing::info!(
            "Launching {:?} with GPU capture to {:?}",
            exe_path,
            output_path
        );

        let mut cmd = Command::new(&pixtool);
        cmd.arg("launch");
        cmd.arg(exe_path);

        // App args / working directory as quoted-value options after the exe.
        if !args.is_empty() {
            push_value_option(&mut cmd, "--command-line", &args.join(" "));
        }
        if let Some(dir) = working_dir {
            push_value_option(&mut cmd, "--working-directory", &dir.display().to_string());
        }

        cmd.arg("take-capture");
        // Bound the capture to N frames so pixtool finishes promptly and tears
        // down the app it launched (matches working pixtool scripts:
        // `take-capture --frames=N`). Without it, take-capture may run until the
        // app exits, hanging the tool call and leaving the process alive.
        if let Some(n) = frames {
            cmd.arg(format!("--frames={}", n));
        }
        cmd.arg("save-capture").arg(output_path);

        let output = cmd
            .output()
            .map_err(|e| anyhow!("Failed to run pixtool: {}", e))?;

        if output.status.success() {
            Ok(CaptureResult {
                output_path: output_path.to_path_buf(),
                message: format!("GPU capture saved to {}", output_path.display()),
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            })
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            Err(anyhow!(
                "pixtool capture failed:\nstderr: {}\nstdout: {}",
                stderr,
                stdout
            ))
        }
    }

    /// Take a timing capture of a running process
    /// Uses: pixtool attach <PID> take-new-timing-capture <file.wpix>
    pub fn timing_capture_process(
        process_id: u32,
        output_path: &Path,
        duration_seconds: Option<u32>,
    ) -> Result<CaptureResult> {
        let pixtool = Self::find()?;

        tracing::info!(
            "Starting timing capture of PID {} to {:?}",
            process_id,
            output_path
        );

        let mut cmd = Command::new(&pixtool);
        cmd.arg("attach")
            .arg(process_id.to_string())
            .arg("take-new-timing-capture")
            .arg(output_path);

        if let Some(duration) = duration_seconds {
            cmd.arg(format!("--duration={}", duration));
        }

        let output = cmd
            .output()
            .map_err(|e| anyhow!("Failed to run pixtool: {}", e))?;

        if output.status.success() {
            Ok(CaptureResult {
                output_path: output_path.to_path_buf(),
                message: format!("Timing capture saved to {}", output_path.display()),
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            })
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            Err(anyhow!(
                "pixtool timing-capture failed:\nstderr: {}\nstdout: {}",
                stderr,
                stdout
            ))
        }
    }

    /// List all capture files in a directory
    pub fn list_captures(dir: &Path) -> Result<Vec<CaptureInfo>> {
        let mut captures = Vec::new();

        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.extension().is_some_and(|e| e == "wpix") {
                let metadata = entry.metadata()?;
                captures.push(CaptureInfo {
                    path: path.clone(),
                    name: path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    size_bytes: metadata.len(),
                    modified: metadata.modified().ok(),
                });
            }
        }

        // Sort by modification time, newest first
        captures.sort_by_key(|c| std::cmp::Reverse(c.modified));

        Ok(captures)
    }
}

/// Result of launching an application
#[derive(Debug, Clone)]
pub struct LaunchResult {
    pub process_id: u32,
    pub message: String,
}

/// Result of a capture operation
#[derive(Debug, Clone)]
pub struct CaptureResult {
    pub output_path: PathBuf,
    pub message: String,
    pub stdout: String,
}

/// Information about a capture file
#[derive(Debug, Clone)]
pub struct CaptureInfo {
    pub path: PathBuf,
    pub name: String,
    pub size_bytes: u64,
    pub modified: Option<std::time::SystemTime>,
}

impl CaptureInfo {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "path": self.path.to_string_lossy(),
            "name": self.name,
            "size_bytes": self.size_bytes,
            "modified": self.modified.map(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            }),
        })
    }
}

/// Build a unique temporary file path to avoid collisions between concurrent
/// pixtool invocations (multiple captures analyzed at once, or several server
/// instances sharing the same temp directory).
pub fn unique_temp_path(prefix: &str, ext: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    std::env::temp_dir().join(format!(
        "{}_{}_{}_{}.{}",
        prefix,
        std::process::id(),
        nanos,
        n,
        ext
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_pixtool() {
        if let Ok(path) = PixTool::find() {
            println!("Found pixtool at: {}", path.display());
            assert!(path.exists());
        }
    }

    #[test]
    fn test_quoted_value_option_wraps_value_in_quotes() {
        // The quotes must wrap the value, not the whole token, so pixtool keeps
        // space-containing values intact (regression test for the 2603.25 bug).
        assert_eq!(
            quoted_value_option(
                "--command-line",
                "+runworld worlds\\RetailSinglePlayer\\c01"
            ),
            "--command-line=\"+runworld worlds\\RetailSinglePlayer\\c01\""
        );
        assert_eq!(
            quoted_value_option("--working-directory", "C:\\Program Files\\My Game"),
            "--working-directory=\"C:\\Program Files\\My Game\""
        );
    }
}
