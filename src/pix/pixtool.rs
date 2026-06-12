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

        // Set working directory via pixtool option (not cmd.current_dir)
        if let Some(dir) = working_dir {
            cmd.arg(format!("--working-directory={}", dir.display()));
        }

        // Set command line args via pixtool option
        if !args.is_empty() {
            cmd.arg(format!("--command-line={}", args.join(" ")));
        }

        cmd.arg(exe_path);

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
                "Launched {} with PIX attached (PID: {})",
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
        // Begin capturing as soon as the app starts rendering.
        cmd.arg("--captureFromStart");

        if let Some(dir) = working_dir {
            cmd.arg(format!("--working-directory={}", dir.display()));
        }

        if !args.is_empty() {
            cmd.arg(format!("--command-line={}", args.join(" ")));
        }

        cmd.arg(exe_path);

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

        // Pass working directory and app arguments via pixtool options so they
        // are not mistaken for pixtool sub-commands (consistent with `launch`).
        if let Some(dir) = working_dir {
            cmd.arg(format!("--working-directory={}", dir.display()));
        }
        if !args.is_empty() {
            cmd.arg(format!("--command-line={}", args.join(" ")));
        }

        cmd.arg(exe_path)
            .arg("take-capture")
            .arg("save-capture")
            .arg(output_path);

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
        captures.sort_by(|a, b| b.modified.cmp(&a.modified));

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
}
