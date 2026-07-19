//! pixtool.exe wrapper for PIX operations
//!
//! This module provides subprocess execution of pixtool.exe for operations
//! that aren't available via the programmatic API.

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Result, anyhow};
use tempfile::{Builder as TempFileBuilder, TempDir, TempPath};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::security;

static PIXTOOL_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);
static PIXTOOL_FOREGROUND_CONCURRENCY: Semaphore = Semaphore::const_new(2);
static PIXTOOL_BACKGROUND_CONCURRENCY: Semaphore = Semaphore::const_new(4);

const PIXTOOL_OPERATION_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const PIXTOOL_LAUNCH_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const PIXTOOL_TIMING_GRACE: Duration = Duration::from_secs(30);
const MAX_PROCESS_OUTPUT_BYTES: usize = 1024 * 1024;
pub(crate) const PROCESS_OUTPUT_DIAGNOSTIC_PREFIX: &str = "[pix-mcp:";
pub(crate) const PROCESS_OUTPUT_TRUNCATION_MARKER: &str = "[pix-mcp: process output truncated]";
const PROCESS_TERMINATION_GRACE: Duration = Duration::from_secs(10);
const PIXTOOL_QUEUE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CAPTURE_DIRECTORY_ENTRIES: usize = 20_000;

#[cfg(windows)]
use process_wrap::tokio::{ChildWrapper, CommandWrap, JobObject};

/// A pixtool child whose process tree is terminated if the owning tool future
/// is cancelled or otherwise dropped before completion.
///
/// On Windows, process-wrap creates the root suspended, assigns it to a Job
/// Object, and only then resumes it. This closes the spawn-to-assignment race
/// that otherwise lets a short-lived launcher escape process-tree cleanup.
/// The local Drop implementation is intentional: process-wrap 9.1.0 issue #35
/// prevents its KillOnDrop wrapper from enabling Job kill-on-close reliably.
struct ManagedChild {
    #[cfg(windows)]
    inner: Box<dyn ChildWrapper>,
    #[cfg(not(windows))]
    inner: tokio::process::Child,
    terminate_on_drop: bool,
}

impl ManagedChild {
    fn spawn(mut command: Command) -> std::io::Result<Self> {
        // If Windows Job Object setup fails after CreateProcess, Tokio still
        // owns the suspended root and must reap it while unwinding the spawn.
        command.kill_on_drop(true);

        #[cfg(windows)]
        let inner = {
            let mut wrapped = CommandWrap::from(command);
            wrapped.wrap(JobObject);
            wrapped.spawn()?
        };
        #[cfg(not(windows))]
        let inner = command.spawn()?;

        Ok(Self {
            inner,
            terminate_on_drop: true,
        })
    }

    fn id(&self) -> Option<u32> {
        self.inner.id()
    }

    fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        #[cfg(windows)]
        {
            self.inner.stdout().take()
        }
        #[cfg(not(windows))]
        {
            self.inner.stdout.take()
        }
    }

    fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        #[cfg(windows)]
        {
            self.inner.stderr().take()
        }
        #[cfg(not(windows))]
        {
            self.inner.stderr.take()
        }
    }

    /// Wait only for the pixtool root. Waiting through JobObjectChild would
    /// also wait for applications that PIX intentionally leaves running.
    async fn wait_root(&mut self) -> std::io::Result<std::process::ExitStatus> {
        #[cfg(windows)]
        {
            self.inner.inner_mut().wait().await
        }
        #[cfg(not(windows))]
        {
            self.inner.wait().await
        }
    }

    fn start_kill_tree(&mut self) -> std::io::Result<()> {
        self.inner.start_kill()
    }

    fn disarm(&mut self) {
        self.terminate_on_drop = false;
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if self.terminate_on_drop
            && let Err(error) = self.start_kill_tree()
        {
            tracing::warn!(%error, "Failed to terminate managed pixtool process tree on drop");
        }
    }
}

async fn terminate_managed_child(child: &mut ManagedChild, operation: &str) {
    if let Err(error) = child.start_kill_tree() {
        tracing::warn!(%error, "Failed to request process-tree termination for {operation}");
    }

    match tokio::time::timeout(PROCESS_TERMINATION_GRACE, child.wait_root()).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            tracing::warn!(%error, "Failed while reaping terminated {operation}");
        }
        Err(_) => {
            tracing::warn!("Timed out while reaping terminated {operation}");
        }
    }
}

/// Tokio detaches a task when its JoinHandle is dropped. Reader tasks own pipe
/// handles, so detaching them during MCP cancellation could keep resources
/// alive indefinitely. This guard makes their lifetime match the tool future.
struct AbortOnDropJoinHandle<T>(tokio::task::JoinHandle<T>);

impl<T> AbortOnDropJoinHandle<T> {
    fn new(handle: tokio::task::JoinHandle<T>) -> Self {
        Self(handle)
    }

    fn abort(&self) {
        self.0.abort();
    }
}

impl<T> Drop for AbortOnDropJoinHandle<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn read_bounded_output<R>(mut reader: R) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut retained = Vec::new();
    let mut chunk = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        let remaining = MAX_PROCESS_OUTPUT_BYTES.saturating_sub(retained.len());
        if remaining > 0 {
            retained.extend_from_slice(&chunk[..read.min(remaining)]);
        }
        truncated |= read > remaining;
    }
    if truncated {
        retained.push(b'\n');
        retained.extend_from_slice(PROCESS_OUTPUT_TRUNCATION_MARKER.as_bytes());
        retained.push(b'\n');
    }
    Ok(retained)
}

async fn join_output_reader(
    mut handle: AbortOnDropJoinHandle<std::io::Result<Vec<u8>>>,
    stream: &str,
) -> Result<Vec<u8>> {
    let result = tokio::time::timeout(PROCESS_TERMINATION_GRACE, &mut handle.0).await;
    let result = match result {
        Ok(result) => result,
        Err(_) => {
            handle.abort();
            tracing::warn!(
                stream,
                "pixtool pipe remained open after the process exited; discarding that stream"
            );
            return Ok(format!(
                "[pix-mcp: {stream} pipe remained open after pixtool exited; output discarded]\n"
            )
            .into_bytes());
        }
    };
    result
        .map_err(|error| anyhow!("{stream} reader task failed: {error}"))?
        .map_err(|error| anyhow!("Failed to read pixtool {stream}: {error}"))
}

/// Wrapper for pixtool.exe operations
pub struct PixTool;

/// Format a `--option="value"` pixtool argument with the value wrapped in
/// literal double quotes.
///
/// pixtool wants the quotes around the *value* (`--option="foo bar"`), not the
/// whole `--option=value` token (`"--option=foo bar"`, which is what
/// `Command::arg` produces on Windows). See [`push_value_option`].
fn quoted_value_option(option: &str, value: &str) -> String {
    // Backslashes immediately before a closing quote must be doubled under
    // Windows command-line parsing rules, otherwise the closing quote itself
    // can be escaped (for example for the perfectly valid directory `C:\\`).
    let trailing_backslashes = value
        .as_bytes()
        .iter()
        .rev()
        .take_while(|&&byte| byte == b'\\')
        .count();
    let mut escaped = String::with_capacity(value.len() + trailing_backslashes);
    escaped.push_str(value);
    for _ in 0..trailing_backslashes {
        escaped.push('\\');
    }
    format!("{option}=\"{escaped}\"")
}

/// Append a `--option="value"` argument to `cmd`, preserving the literal quotes
/// around the value in the actual command line.
///
/// On Windows this needs `raw_arg`: `Command::arg` would quote the entire
/// `--option=value` token (`"--option=foo bar"`) instead of just the value
/// (`--option="foo bar"`), which pixtool mishandles.
#[cfg(windows)]
fn push_value_option_unchecked(cmd: &mut Command, option: &str, value: &str) {
    cmd.raw_arg(quoted_value_option(option, value));
}

/// Non-Windows fallback (pixtool is Windows-only; this keeps the crate building
/// on other platforms for development and tests).
#[cfg(not(windows))]
fn push_value_option_unchecked(cmd: &mut Command, option: &str, value: &str) {
    cmd.arg(format!("{option}={value}"));
}

/// pixtool 2603.25 rejects a `--command-line` value that contains a space or
/// starts with `-`/`+` (verified). Detect that so callers can warn the agent
/// and suggest the documented workarounds (`autoexec.cfg`, or env vars).
fn command_line_unsupported(value: &str) -> bool {
    value.contains(' ') || value.starts_with('-') || value.starts_with('+')
}

fn validate_raw_value(label: &str, value: &str) -> Result<()> {
    if value.contains('"') || value.chars().any(char::is_control) {
        return Err(anyhow!(
            "{label} contains a quote or control character, which cannot be passed safely to \
             pixtool"
        ));
    }
    Ok(())
}

/// Add a pixtool `--option="value"` token using the quoting layout expected by
/// the PIX parser. Values are validated before `raw_arg` is used.
pub(crate) fn push_value_option(cmd: &mut Command, option: &str, value: &str) -> Result<()> {
    validate_raw_value(option, value)?;
    push_value_option_unchecked(cmd, option, value);
    Ok(())
}

/// Validate the very limited `--command-line` syntax accepted by pixtool
/// 2603.25 before launching anything. Sending unsupported values and warning
/// afterwards is unsafe: the target may already have started with the wrong
/// configuration.
fn validate_command_line_args(args: &[&str]) -> Result<Option<String>> {
    if args.is_empty() {
        return Ok(None);
    }

    for value in args {
        if value.is_empty() {
            return Err(anyhow!("Application arguments must not be empty"));
        }
        validate_raw_value("Application argument", value)?;
    }

    let joined = args.join(" ");
    if command_line_unsupported(&joined) {
        return Err(anyhow!(
            "pixtool 2603.25 cannot safely pass these application arguments via \
             --command-line: {joined:?}. Values containing spaces and values starting with \
             '-' or '+' are rejected. Use the application's config/autoexec file or an \
             environment variable instead."
        ));
    }

    Ok(Some(joined))
}

fn path_is_blank(path: &Path) -> bool {
    path.as_os_str().is_empty() || path.to_string_lossy().trim().is_empty()
}

fn has_extension_ignore_ascii_case(path: &Path, expected: &str) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case(expected))
}

fn validate_executable(path: &Path) -> Result<PathBuf> {
    security::validate_executable(path)
}

fn validate_working_directory(path: Option<&Path>) -> Result<Option<PathBuf>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let path = security::validate_working_directory(path)?;
    validate_raw_value("Working directory", &path.to_string_lossy())?;
    Ok(Some(path))
}

fn validate_capture_input(path: &Path) -> Result<PathBuf> {
    if !has_extension_ignore_ascii_case(path, "wpix") {
        return Err(anyhow!(
            "Capture path must have a .wpix extension: {}",
            path.display()
        ));
    }
    let path = security::validate_input_file(path, "Capture path")?;
    if !has_extension_ignore_ascii_case(&path, "wpix") {
        return Err(anyhow!(
            "Capture path resolves to a file without a .wpix extension: {}",
            path.display()
        ));
    }
    Ok(path)
}

fn validate_new_capture_output(path: &Path) -> Result<PathBuf> {
    if path_is_blank(path) || path.file_name().is_none() {
        return Err(anyhow!("Capture output path must not be empty"));
    }
    if !has_extension_ignore_ascii_case(path, "wpix") {
        return Err(anyhow!(
            "Capture output path must have a .wpix extension: {}",
            path.display()
        ));
    }

    let path = security::validate_output_file(path, "Capture output path")?;
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            return Err(anyhow!(
                "Capture output already exists; choose a new path: {}",
                path.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(anyhow!(
                "Cannot inspect capture output {}: {}",
                path.display(),
                error
            ));
        }
    }

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(anyhow!(
            "Capture output parent does not exist or is not a directory: {}",
            parent.display()
        ));
    }
    Ok(path)
}

fn verify_new_capture_output(path: &Path) -> Result<()> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        anyhow!(
            "pixtool reported success but did not create {}: {}",
            path.display(),
            error
        )
    })?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(anyhow!(
            "pixtool did not create a non-empty capture file at {}",
            path.display()
        ));
    }
    Ok(())
}

/// Owns a unique same-directory output while pixtool is running. The requested
/// destination is never touched until the temporary capture has been validated,
/// and `persist_noclobber` prevents concurrent requests from overwriting it.
struct PendingCaptureOutput {
    destination: PathBuf,
    _directory: TempDir,
    temporary: TempPath,
}

impl PendingCaptureOutput {
    fn new(destination: &Path) -> Result<Self> {
        let destination = validate_new_capture_output(destination)?;
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let directory = TempFileBuilder::new()
            .prefix(".pix-mcp-capture-")
            .tempdir_in(parent)?;
        let file = TempFileBuilder::new()
            .prefix("output-")
            .suffix(".wpix")
            .tempfile_in(directory.path())?;
        let temporary = file.into_temp_path();
        // pixtool creates its output itself. TempPath retains ownership of the
        // randomized name and removes any partial file on cancellation/error.
        std::fs::remove_file(&temporary)?;
        Ok(Self {
            destination,
            _directory: directory,
            temporary,
        })
    }

    fn path(&self) -> &Path {
        self.temporary.as_ref()
    }

    fn verify_and_persist(self) -> Result<PathBuf> {
        verify_new_capture_output(&self.temporary)?;
        let destination = self.destination.clone();
        self.temporary
            .persist_noclobber(&destination)
            .map_err(|error| {
                anyhow!(
                    "Failed to persist capture to {} without overwriting an existing file: {}",
                    destination.display(),
                    error
                )
            })?;
        Ok(destination)
    }
}

fn validate_process_id(process_id: u32) -> Result<()> {
    if process_id == 0 {
        return Err(anyhow!("process_id must be greater than zero"));
    }
    Ok(())
}

fn validate_frames(frames: Option<u32>) -> Result<u32> {
    let frames = frames.unwrap_or(1);
    if !(1..=120).contains(&frames) {
        return Err(anyhow!("frames must be between 1 and 120"));
    }
    Ok(frames)
}

fn validate_duration(duration_ms: Option<u32>) -> Result<u32> {
    let duration_ms = duration_ms.unwrap_or(100);
    if !(1..=600_000).contains(&duration_ms) {
        return Err(anyhow!("duration_ms must be between 1 and 600000"));
    }
    Ok(duration_ms)
}

/// Run a pixtool command under the server-wide concurrency and timeout policy.
///
/// This is public within the crate's PIX integration surface so analysis tools
/// can use the same process lifecycle guarantees without making `PixTool::find`
/// asynchronous.
pub async fn run_pixtool_command(
    command: Command,
    timeout_duration: Duration,
    operation: &str,
) -> Result<Output> {
    run_pixtool_command_with_pid(command, timeout_duration, operation)
        .await
        .map(|(_, output)| output)
}

async fn run_pixtool_command_with_pid(
    mut command: Command,
    timeout_duration: Duration,
    operation: &str,
) -> Result<(u32, Output)> {
    let _permit = tokio::time::timeout(
        PIXTOOL_QUEUE_TIMEOUT,
        PIXTOOL_FOREGROUND_CONCURRENCY.acquire(),
    )
    .await
    .map_err(|_| {
        anyhow!(
            "{operation} waited {} seconds for a pixtool execution slot",
            PIXTOOL_QUEUE_TIMEOUT.as_secs()
        )
    })?
    .map_err(|_| anyhow!("pixtool concurrency limiter is closed"))?;

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ManagedChild::spawn(command)
        .map_err(|error| anyhow!("Failed to start {operation}: {error}"))?;
    let process_id = child
        .id()
        .ok_or_else(|| anyhow!("{operation} started without a process ID"))?;
    let stdout = child
        .take_stdout()
        .ok_or_else(|| anyhow!("{operation} stdout pipe was not available"))?;
    let stderr = child
        .take_stderr()
        .ok_or_else(|| anyhow!("{operation} stderr pipe was not available"))?;
    let mut stdout_reader = AbortOnDropJoinHandle::new(tokio::spawn(read_bounded_output(stdout)));
    let mut stderr_reader = AbortOnDropJoinHandle::new(tokio::spawn(read_bounded_output(stderr)));

    let status = match tokio::time::timeout(timeout_duration, child.wait_root()).await {
        Ok(result) => {
            result.map_err(|error| anyhow!("Failed while waiting for {operation}: {error}"))?
        }
        Err(_) => {
            terminate_managed_child(&mut child, operation).await;
            if tokio::time::timeout(PROCESS_TERMINATION_GRACE, &mut stdout_reader.0)
                .await
                .is_err()
            {
                stdout_reader.abort();
            }
            if tokio::time::timeout(PROCESS_TERMINATION_GRACE, &mut stderr_reader.0)
                .await
                .is_err()
            {
                stderr_reader.abort();
            }
            return Err(anyhow!(
                "{operation} timed out after {} seconds and was terminated",
                timeout_duration.as_secs()
            ));
        }
    };
    // Keep the bounded raw streams for internal parsers and classifiers.
    // Sanitization is applied only when diagnostics cross the MCP boundary;
    // truncating here would silently discard counters or late replay errors.
    let stdout = join_output_reader(stdout_reader, "stdout").await?;
    let stderr = join_output_reader(stderr_reader, "stderr").await?;
    if status.success() {
        child.disarm();
    }
    Ok((
        process_id,
        Output {
            status,
            stdout,
            stderr,
        },
    ))
}

fn public_process_output(bytes: &[u8]) -> String {
    security::sanitize_process_output(&String::from_utf8_lossy(bytes))
}

async fn spawn_background_pixtool(mut command: Command, operation: &'static str) -> Result<u32> {
    let permit = PIXTOOL_BACKGROUND_CONCURRENCY
        .try_acquire()
        .map_err(|_| anyhow!("Too many background PIX launches are already running (maximum 4)"))?;

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = ManagedChild::spawn(command)
        .map_err(|error| anyhow!("Failed to start {operation}: {error}"))?;
    let process_id = child
        .id()
        .ok_or_else(|| anyhow!("{operation} started without a process ID"))?;
    // Catch deterministic startup failures before reporting the fire-and-forget
    // launch as successful. A still-running process is handed to the background
    // monitor below.
    if let Ok(wait_result) =
        tokio::time::timeout(Duration::from_millis(250), child.wait_root()).await
    {
        let status =
            wait_result.map_err(|error| anyhow!("Failed while starting {operation}: {error}"))?;
        if !status.success() {
            return Err(anyhow!("{operation} exited immediately with {status}"));
        }
        child.disarm();
        return Ok(process_id);
    }

    tokio::spawn(async move {
        let _permit = permit;
        match tokio::time::timeout(PIXTOOL_LAUNCH_TIMEOUT, child.wait_root()).await {
            Ok(Ok(status)) => {
                if status.success() {
                    child.disarm();
                } else {
                    tracing::warn!(%status, "{operation} exited unsuccessfully");
                }
            }
            Ok(Err(error)) => tracing::warn!(%error, "Failed while waiting for {operation}"),
            Err(_) => {
                tracing::warn!("{operation} timed out and will be terminated");
                terminate_managed_child(&mut child, operation).await;
            }
        }
    });

    Ok(process_id)
}

fn natural_compare(left: &OsStr, right: &OsStr) -> Ordering {
    let left = left.to_string_lossy();
    let right = right.to_string_lossy();
    let left = left.as_bytes();
    let right = right.as_bytes();
    let (mut left_index, mut right_index) = (0, 0);

    while left_index < left.len() && right_index < right.len() {
        if left[left_index].is_ascii_digit() && right[right_index].is_ascii_digit() {
            let left_start = left_index;
            let right_start = right_index;
            while left_index < left.len() && left[left_index].is_ascii_digit() {
                left_index += 1;
            }
            while right_index < right.len() && right[right_index].is_ascii_digit() {
                right_index += 1;
            }

            let left_digits = &left[left_start..left_index];
            let right_digits = &right[right_start..right_index];
            let left_significant = left_digits
                .iter()
                .position(|digit| *digit != b'0')
                .map_or(&left_digits[left_digits.len()..], |index| {
                    &left_digits[index..]
                });
            let right_significant = right_digits
                .iter()
                .position(|digit| *digit != b'0')
                .map_or(&right_digits[right_digits.len()..], |index| {
                    &right_digits[index..]
                });

            let ordering = left_significant
                .len()
                .cmp(&right_significant.len())
                .then_with(|| left_significant.cmp(right_significant))
                .then_with(|| left_digits.len().cmp(&right_digits.len()));
            if ordering != Ordering::Equal {
                return ordering;
            }
        } else {
            let ordering = left[left_index]
                .to_ascii_lowercase()
                .cmp(&right[right_index].to_ascii_lowercase());
            if ordering != Ordering::Equal {
                return ordering;
            }
            left_index += 1;
            right_index += 1;
        }
    }

    left.len().cmp(&right.len())
}

/// pixtool analysis/playback verbs (save-event-list, save-resource,
/// save-screenshot, list-counters, run-debug-layer, ...) fail without Windows
/// Developer Mode. Map that failure to actionable guidance.
pub fn check_developer_mode(stdout: &str, stderr: &str) -> Result<()> {
    let combined = format!("{}\n{}", stdout, stderr).to_lowercase();
    if combined.contains("developer mode") || combined.contains("requires_developer_mode") {
        return Err(anyhow!(
            "pixtool analysis requires Windows Developer Mode. Enable it (Settings → For \
             developers → Developer Mode) and retry. Capturing does not need it; only \
             open-capture analysis (event lists, screenshots, counters) does."
        ));
    }
    Ok(())
}

impl PixTool {
    /// Find pixtool.exe in the PIX installation
    pub fn find() -> Result<PathBuf> {
        {
            let mut guard = PIXTOOL_PATH
                .lock()
                .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
            if let Some(path) = guard.as_ref() {
                if path.is_file() {
                    return Ok(path.clone());
                }
                // PIX may have been upgraded or removed since the last call.
                *guard = None;
            }
        }

        // Check environment variable first
        if let Ok(raw_path) = std::env::var("PIXTOOL_PATH") {
            let path = PathBuf::from(raw_path);
            if path.is_file() {
                let mut guard = PIXTOOL_PATH
                    .lock()
                    .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
                *guard = Some(path.clone());
                return Ok(path);
            }
            tracing::warn!(
                "PIXTOOL_PATH does not point to a file; searching installed PIX versions"
            );
        }

        // Search in Program Files
        let program_files =
            std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".to_string());

        let pix_dir = PathBuf::from(&program_files).join("Microsoft PIX");

        if pix_dir.is_dir() {
            // Find the latest version
            let mut versions: Vec<_> = std::fs::read_dir(&pix_dir)?
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .collect();

            versions.sort_by(|left, right| {
                natural_compare(&right.file_name(), &left.file_name())
                    .then_with(|| right.file_name().cmp(&left.file_name()))
            });

            for version in versions {
                let tool_path = version.path().join("pixtool.exe");
                if tool_path.is_file() {
                    let mut guard = PIXTOOL_PATH
                        .lock()
                        .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
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
    pub async fn launch(
        exe_path: &Path,
        args: &[&str],
        working_dir: Option<&Path>,
    ) -> Result<LaunchResult> {
        security::ensure_user_launch_allowed()?;
        let exe_path = validate_executable(exe_path)?;
        let working_dir = validate_working_directory(working_dir)?;
        let command_line = validate_command_line_args(args)?;
        let pixtool = Self::find()?;

        let mut cmd = Command::new(&pixtool);
        cmd.arg("launch");
        cmd.arg(&exe_path);

        // Pass app args and working directory as quoted-value options *after*
        // the exe, matching the form pixtool accepts (quotes around the value).
        if let Some(command_line) = command_line.as_deref() {
            push_value_option(&mut cmd, "--command-line", command_line)?;
        }
        if let Some(dir) = working_dir.as_deref() {
            push_value_option(&mut cmd, "--working-directory", &dir.display().to_string())?;
        }

        tracing::info!("Launching a policy-approved executable via pixtool");

        let pid = spawn_background_pixtool(cmd, "pixtool launch").await?;

        let message = format!(
            "Launched {} under pixtool (pixtool PID: {} — this is the launcher process, not \
             the game). For a programmatic GPU capture use pix_gpu_capture_launch or \
             pix_capture_and_analyze: PIX can only GPU-capture a process it launched itself, \
             so attaching by PID to a separately-started game will fail.",
            exe_path.display(),
            pid
        );

        Ok(LaunchResult {
            process_id: pid,
            message,
        })
    }

    /// Launch an application with PIX and immediately start capturing
    pub async fn launch_and_capture(
        exe_path: &Path,
        args: &[&str],
        capture_file: Option<&Path>,
        working_dir: Option<&Path>,
    ) -> Result<LaunchResult> {
        security::ensure_user_launch_allowed()?;
        let exe_path = validate_executable(exe_path)?;
        let working_dir = validate_working_directory(working_dir)?;
        let command_line = validate_command_line_args(args)?;
        let mut pending_output = capture_file.map(PendingCaptureOutput::new).transpose()?;
        let pixtool = Self::find()?;

        let mut cmd = Command::new(&pixtool);
        cmd.arg("launch");
        cmd.arg(&exe_path);
        // Begin capturing as soon as the app starts rendering.
        cmd.arg("--captureFromStart");

        if let Some(command_line) = command_line.as_deref() {
            push_value_option(&mut cmd, "--command-line", command_line)?;
        }
        if let Some(dir) = working_dir.as_deref() {
            push_value_option(&mut cmd, "--working-directory", &dir.display().to_string())?;
        }

        // Acquire the frame, then optionally save it. --captureFromStart only
        // arms capture before the app runs; take-capture is what actually
        // records a frame (per the pixtool 2603.25 reference).
        cmd.arg("take-capture").arg("--frames=1");
        if capture_file.is_some() {
            cmd.arg("save-capture");
            cmd.arg(
                pending_output
                    .as_ref()
                    .expect("capture destination has a pending-output guard")
                    .path(),
            );
        } else {
            // Without a destination, explicitly ask PIX to open the completed
            // capture rather than implying that an unsaved artifact exists.
            cmd.arg("--open");
        }

        tracing::info!("Launching a policy-approved executable with capture via pixtool");

        let (pid, message) = match capture_file {
            Some(_) => {
                let (pid, output) = run_pixtool_command_with_pid(
                    cmd,
                    PIXTOOL_OPERATION_TIMEOUT,
                    "pixtool launch-and-capture",
                )
                .await?;
                if !output.status.success() {
                    return Err(anyhow!(
                        "pixtool launch-and-capture failed:\nstderr: {}\nstdout: {}",
                        public_process_output(&output.stderr),
                        public_process_output(&output.stdout)
                    ));
                }
                let file = pending_output
                    .take()
                    .expect("capture destination has a pending-output guard")
                    .verify_and_persist()?;
                (
                    pid,
                    format!(
                        "Captured {} from launch and saved a non-empty capture to {} \
                         (pixtool launcher PID was {})",
                        exe_path.display(),
                        file.display(),
                        pid
                    ),
                )
            }
            None => {
                let pid = spawn_background_pixtool(cmd, "pixtool launch-and-open-capture").await?;
                (
                    pid,
                    format!(
                        "Started {} under PIX and requested a one-frame capture to open in the \
                         PIX GUI (pixtool launcher PID: {})",
                        exe_path.display(),
                        pid
                    ),
                )
            }
        };

        Ok(LaunchResult {
            process_id: pid,
            message,
        })
    }

    /// Open a capture file in the PIX GUI
    pub async fn open_capture(capture_path: &Path) -> Result<()> {
        let capture_path = validate_capture_input(capture_path)?;
        // Find WinPix.exe (GUI) in same directory as pixtool
        let pixtool = Self::find()?;
        let pix_dir = pixtool
            .parent()
            .ok_or_else(|| anyhow!("Cannot find PIX directory"))?;
        let winpix = pix_dir.join("WinPix.exe");

        if !winpix.is_file() {
            return Err(anyhow!("WinPix.exe not found at: {}", winpix.display()));
        }

        let mut command = Command::new(&winpix);
        command
            .arg(&capture_path)
            .kill_on_drop(false)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command
            .spawn()
            .map_err(|e| anyhow!("Failed to launch WinPix: {}", e))?;

        // Reap asynchronously without holding an OS thread. WinPix is a GUI
        // process and intentionally remains alive if the MCP server exits.
        tokio::spawn(async move {
            if let Err(error) = child.wait().await {
                tracing::warn!(%error, "Failed while waiting for WinPix");
            }
        });

        Ok(())
    }

    /// Take a GPU capture of a running process
    /// Uses: pixtool attach <PID> take-capture save-capture <file.wpix>
    pub async fn gpu_capture_process(process_id: u32, output_path: &Path) -> Result<CaptureResult> {
        validate_process_id(process_id)?;
        let pending_output = PendingCaptureOutput::new(output_path)?;
        let pixtool = Self::find()?;

        tracing::info!(process_id, "Starting GPU capture");

        let mut command = Command::new(&pixtool);
        command
            .arg("attach")
            .arg(process_id.to_string())
            .arg("take-capture")
            .arg("save-capture")
            .arg(pending_output.path());
        let output =
            run_pixtool_command(command, PIXTOOL_OPERATION_TIMEOUT, "pixtool GPU capture").await?;

        if output.status.success() {
            let output_path = pending_output.verify_and_persist()?;
            Ok(CaptureResult {
                output_path: output_path.clone(),
                message: format!("GPU capture saved to {}", output_path.display()),
                stdout: public_process_output(&output.stdout),
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
                    security::sanitize_process_output(&stdout),
                    security::sanitize_process_output(&stderr)
                ));
            }
            Err(anyhow!(
                "pixtool capture failed:\nstderr: {}\nstdout: {}",
                security::sanitize_process_output(&stderr),
                security::sanitize_process_output(&stdout)
            ))
        }
    }

    /// Launch executable and capture, then save
    /// Uses: pixtool launch <exe> take-capture save-capture <file.wpix>
    pub async fn gpu_capture_launch(
        exe_path: &Path,
        args: &[&str],
        output_path: &Path,
        working_dir: Option<&Path>,
        frames: Option<u32>,
    ) -> Result<CaptureResult> {
        security::ensure_user_launch_allowed()?;
        let exe_path = validate_executable(exe_path)?;
        let working_dir = validate_working_directory(working_dir)?;
        let pending_output = PendingCaptureOutput::new(output_path)?;
        let command_line = validate_command_line_args(args)?;
        let frames = validate_frames(frames)?;
        let pixtool = Self::find()?;

        tracing::info!("Launching a policy-approved executable with GPU capture");

        let mut cmd = Command::new(&pixtool);
        cmd.arg("launch");
        cmd.arg(&exe_path);

        // App args / working directory as quoted-value options after the exe.
        if let Some(command_line) = command_line.as_deref() {
            push_value_option(&mut cmd, "--command-line", command_line)?;
        }
        if let Some(dir) = working_dir.as_deref() {
            push_value_option(&mut cmd, "--working-directory", &dir.display().to_string())?;
        }

        cmd.arg("take-capture");
        // Bound the capture to N frames so pixtool finishes promptly and tears
        // down the app it launched (matches working pixtool scripts:
        // `take-capture --frames=N`). Without it, take-capture may run until the
        // app exits, hanging the tool call and leaving the process alive.
        cmd.arg(format!("--frames={}", frames));
        cmd.arg("save-capture").arg(pending_output.path());

        let output =
            run_pixtool_command(cmd, PIXTOOL_OPERATION_TIMEOUT, "pixtool launch GPU capture")
                .await?;

        if output.status.success() {
            let output_path = pending_output.verify_and_persist()?;
            let message = format!("GPU capture saved to {}", output_path.display());
            Ok(CaptureResult {
                output_path,
                message,
                stdout: public_process_output(&output.stdout),
            })
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            check_developer_mode(&stdout, &stderr)?;
            Err(anyhow!(
                "pixtool capture failed:\nstderr: {}\nstdout: {}",
                security::sanitize_process_output(&stderr),
                security::sanitize_process_output(&stdout)
            ))
        }
    }

    /// Take a timing capture of a running process
    /// Uses: pixtool attach <PID> take-new-timing-capture <file.wpix> [--duration=<ms>]
    pub async fn timing_capture_process(
        process_id: u32,
        output_path: &Path,
        duration_ms: Option<u32>,
    ) -> Result<CaptureResult> {
        validate_process_id(process_id)?;
        let pending_output = PendingCaptureOutput::new(output_path)?;
        let duration_ms = validate_duration(duration_ms)?;
        let pixtool = Self::find()?;

        tracing::info!(process_id, "Starting timing capture");

        let mut cmd = Command::new(&pixtool);
        cmd.arg("attach")
            .arg(process_id.to_string())
            .arg("take-new-timing-capture")
            .arg(pending_output.path());

        // pixtool's --duration is in milliseconds (default 100), not seconds.
        cmd.arg(format!("--duration={}", duration_ms));

        let timeout_duration =
            Duration::from_millis(u64::from(duration_ms)).saturating_add(PIXTOOL_TIMING_GRACE);
        let output = run_pixtool_command(cmd, timeout_duration, "pixtool timing capture").await?;

        if output.status.success() {
            let output_path = pending_output.verify_and_persist()?;
            Ok(CaptureResult {
                output_path: output_path.clone(),
                message: format!("Timing capture saved to {}", output_path.display()),
                stdout: public_process_output(&output.stdout),
            })
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            Err(anyhow!(
                "pixtool timing-capture failed:\nstderr: {}\nstdout: {}",
                security::sanitize_process_output(&stderr),
                security::sanitize_process_output(&stdout)
            ))
        }
    }

    /// List all capture files in a directory
    pub fn list_captures(dir: &Path) -> Result<Vec<CaptureInfo>> {
        let dir = security::validate_input_directory(dir, "Capture directory")?;
        let mut captures = Vec::new();

        for (index, entry) in std::fs::read_dir(&dir)?.enumerate() {
            if index >= MAX_CAPTURE_DIRECTORY_ENTRIES {
                return Err(anyhow!(
                    "Capture directory contains more than {} entries; choose a narrower directory",
                    MAX_CAPTURE_DIRECTORY_ENTRIES
                ));
            }
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let path = entry.path();

            if has_extension_ignore_ascii_case(&path, "wpix") {
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
        captures.sort_by(|left, right| {
            right
                .modified
                .cmp(&left.modified)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                .then_with(|| left.name.cmp(&right.name))
        });

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_pixtool() {
        if let Ok(path) = PixTool::find() {
            println!("Found pixtool at: {}", path.display());
            assert!(path.is_file());
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
        assert_eq!(
            quoted_value_option("--working-directory", "C:\\"),
            "--working-directory=\"C:\\\\\""
        );
    }

    #[test]
    fn test_command_line_unsupported_detects_spaces_and_leading_dash_plus() {
        // pixtool 2603.25 rejects values with spaces or starting with -/+.
        assert!(command_line_unsupported("foo bar"));
        assert!(command_line_unsupported("-windowed"));
        assert!(command_line_unsupported("+runworld"));
        assert!(!command_line_unsupported("foobar"));
        assert!(!command_line_unsupported("level1"));
    }

    #[test]
    fn test_command_line_validation_rejects_unsupported_or_unsafe_values() {
        assert_eq!(
            validate_command_line_args(&[]).expect("empty is valid"),
            None
        );
        assert_eq!(
            validate_command_line_args(&["level1"]).expect("single token is valid"),
            Some("level1".to_string())
        );
        assert!(validate_command_line_args(&["foo", "bar"]).is_err());
        assert!(validate_command_line_args(&["-windowed"]).is_err());
        assert!(validate_command_line_args(&["bad\"arg"]).is_err());
        assert!(validate_command_line_args(&["bad\narg"]).is_err());
    }

    #[test]
    fn test_natural_compare_orders_numeric_pix_versions() {
        assert_eq!(
            natural_compare(OsStr::new("2603.25"), OsStr::new("2603.9")),
            Ordering::Greater
        );
        assert_eq!(
            natural_compare(OsStr::new("2509.1"), OsStr::new("2603.1")),
            Ordering::Less
        );
    }

    #[test]
    fn test_check_developer_mode_maps_error() {
        assert!(check_developer_mode("ok", "").is_ok());
        assert!(check_developer_mode("", "E_PIX_FEATURE_REQUIRES_DEVELOPER_MODE").is_err());
        assert!(check_developer_mode("Please enable Developer Mode", "").is_err());
    }

    #[test]
    fn capture_output_is_staged_before_it_is_persisted() {
        let directory = tempfile::tempdir().expect("test directory");
        let destination = directory.path().join("capture.wpix");
        let pending = PendingCaptureOutput::new(&destination).expect("pending capture");
        let staging_path = pending.path().to_path_buf();

        assert_ne!(staging_path, destination);
        assert!(!destination.exists());
        assert!(!staging_path.exists());

        std::fs::write(&staging_path, b"capture data").expect("write staged capture");
        pending
            .verify_and_persist()
            .expect("persist staged capture");

        assert_eq!(
            std::fs::read(&destination).expect("read persisted capture"),
            b"capture data"
        );
        assert!(!staging_path.exists());
    }

    #[test]
    fn capture_output_does_not_clobber_a_racing_destination() {
        let directory = tempfile::tempdir().expect("test directory");
        let destination = directory.path().join("capture.wpix");
        let pending = PendingCaptureOutput::new(&destination).expect("pending capture");

        std::fs::write(pending.path(), b"new capture").expect("write staged capture");
        std::fs::write(&destination, b"existing capture").expect("create racing destination");

        let error = pending
            .verify_and_persist()
            .expect_err("existing destination must not be overwritten");
        assert!(error.to_string().contains("without overwriting"));
        assert_eq!(
            std::fs::read(destination).expect("read existing capture"),
            b"existing capture"
        );
    }

    #[tokio::test]
    async fn bounded_output_drains_but_does_not_retain_unbounded_data() {
        use tokio::io::AsyncWriteExt;

        let (mut writer, reader) = tokio::io::duplex(64 * 1024);
        let payload = vec![b'x'; MAX_PROCESS_OUTPUT_BYTES + 4096];
        let writer_task = tokio::spawn(async move {
            writer.write_all(&payload).await.expect("write output");
            writer.shutdown().await.expect("close output");
        });

        let output = read_bounded_output(reader).await.expect("read output");
        writer_task.await.expect("writer task");
        assert!(output.starts_with(&vec![b'x'; 1024]));
        assert_eq!(
            output.len(),
            MAX_PROCESS_OUTPUT_BYTES + PROCESS_OUTPUT_TRUNCATION_MARKER.len() + 2
        );
        assert!(output.ends_with(format!("{PROCESS_OUTPUT_TRUNCATION_MARKER}\n").as_bytes()));
    }

    #[tokio::test]
    async fn abort_on_drop_join_handle_cancels_detached_tasks() {
        struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for DropSignal {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let _signal = DropSignal(Some(dropped_tx));
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
        started_rx.await.expect("task started");

        drop(AbortOnDropJoinHandle::new(handle));
        tokio::time::timeout(Duration::from_secs(1), dropped_rx)
            .await
            .expect("task should be aborted")
            .expect("drop signal");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn managed_child_terminates_its_process_tree() {
        use windows::Win32::{
            Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
        };

        let temp = tempfile::tempdir().expect("create test directory");
        let descendant_pid_file = temp.path().join("descendant.pid");
        let mut command = Command::new("powershell.exe");
        command
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
            .arg(
                "$ErrorActionPreference = 'Stop'; \
                 $child = Start-Process -FilePath \"$env:SystemRoot\\System32\\ping.exe\" \
                    -ArgumentList '-t','127.0.0.1' -WindowStyle Hidden -PassThru; \
                 [IO.File]::WriteAllText($env:PIX_MCP_CHILD_PID_FILE, [string]$child.Id); \
                 Wait-Process -Id $child.Id",
            )
            .env("PIX_MCP_CHILD_PID_FILE", &descendant_pid_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = ManagedChild::spawn(command).expect("spawn managed process");

        let descendant_pid = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(&descendant_pid_file)
                    && let Ok(pid) = pid.trim().parse::<u32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("descendant PID should be published");
        let descendant = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, descendant_pid) }
            .expect("open descendant process");
        assert_eq!(unsafe { WaitForSingleObject(descendant, 0) }, WAIT_TIMEOUT);

        // Exercise the same RAII path used when an MCP tool future is dropped
        // by a cancellation notification.
        drop(child);
        assert_eq!(
            unsafe {
                WaitForSingleObject(descendant, PROCESS_TERMINATION_GRACE.as_millis() as u32)
            },
            WAIT_OBJECT_0,
            "the descendant must terminate with the managed root"
        );
        unsafe {
            CloseHandle(descendant).expect("close descendant process handle");
        }
    }
}
