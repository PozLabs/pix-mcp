//! Local security policy for filesystem access and user-controlled launches.
//!
//! PIX is inherently a local, privileged debugging tool. This module keeps
//! that trust boundary explicit and applies one policy consistently across
//! captures, analysis artifacts, MCP resources, and executable launches.

use std::env;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};

const DEFAULT_MAX_CONCURRENT_TOOLS: usize = 8;
const MAX_CONFIGURED_CONCURRENT_TOOLS: usize = 64;
const MAX_PUBLIC_DIAGNOSTIC_BYTES: usize = 32 * 1024;
const DIAGNOSTIC_TRUNCATION_MARKER: &str = "\n[pix-mcp: diagnostic output truncated]\n";

static POLICY: OnceLock<SecurityPolicy> = OnceLock::new();

#[derive(Debug, Clone)]
pub struct SecurityPolicy {
    captures_dir: PathBuf,
    input_roots: Option<Vec<PathBuf>>,
    output_roots: Option<Vec<PathBuf>>,
    executable_roots: Option<Vec<PathBuf>>,
    allow_unc_paths: bool,
    allow_elevated_launch: bool,
    max_concurrent_tools: usize,
}

impl SecurityPolicy {
    fn from_env() -> Result<Self> {
        Self::from_lookup(|name| match env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(env::VarError::NotPresent) => Ok(None),
            Err(env::VarError::NotUnicode(_)) => Err(anyhow!("{name} contains non-Unicode data")),
        })
    }

    fn from_lookup<F>(lookup: F) -> Result<Self>
    where
        F: Fn(&str) -> Result<Option<String>>,
    {
        let allow_unc_paths = parse_bool_setting(
            "PIX_MCP_ALLOW_UNC_PATHS",
            lookup("PIX_MCP_ALLOW_UNC_PATHS")?,
            false,
        )?;
        let allow_elevated_launch = parse_bool_setting(
            "PIX_MCP_ALLOW_ELEVATED_LAUNCH",
            lookup("PIX_MCP_ALLOW_ELEVATED_LAUNCH")?,
            false,
        )?;
        let max_concurrent_tools = parse_concurrency(lookup("PIX_MCP_MAX_CONCURRENT_TOOLS")?)?;

        let captures_dir_raw = match lookup("PIX_MCP_CAPTURES_DIR")? {
            Some(value) if value.trim().is_empty() => {
                return Err(anyhow!("PIX_MCP_CAPTURES_DIR must not be empty"));
            }
            Some(value) => PathBuf::from(value),
            None => env::current_dir().context("Could not determine the current directory")?,
        };
        let captures_dir =
            canonical_directory(&captures_dir_raw, "PIX_MCP_CAPTURES_DIR", allow_unc_paths)?;

        let input_roots = parse_roots(
            "PIX_MCP_INPUT_ROOTS",
            lookup("PIX_MCP_INPUT_ROOTS")?,
            Some(&captures_dir),
            allow_unc_paths,
        )?;
        let output_roots = parse_roots(
            "PIX_MCP_OUTPUT_ROOTS",
            lookup("PIX_MCP_OUTPUT_ROOTS")?,
            Some(&captures_dir),
            allow_unc_paths,
        )?;
        let executable_roots = parse_roots(
            "PIX_MCP_EXECUTABLE_ROOTS",
            lookup("PIX_MCP_EXECUTABLE_ROOTS")?,
            None,
            allow_unc_paths,
        )?;

        Ok(Self {
            captures_dir,
            input_roots,
            output_roots,
            executable_roots,
            allow_unc_paths,
            allow_elevated_launch,
            max_concurrent_tools,
        })
    }

    fn validate_existing(
        &self,
        path: &Path,
        label: &str,
        expected: ExpectedPath,
        roots: Option<&[PathBuf]>,
    ) -> Result<PathBuf> {
        validate_path_syntax(path, label, self.allow_unc_paths)?;
        let canonical = canonicalize(path)
            .with_context(|| format!("{label} does not exist: {}", path.display()))?;
        validate_path_syntax(&canonical, label, self.allow_unc_paths)?;

        let metadata = std::fs::metadata(&canonical)
            .with_context(|| format!("Cannot inspect {label}: {}", canonical.display()))?;
        let expected_matches = match expected {
            ExpectedPath::File => metadata.is_file(),
            ExpectedPath::Directory => metadata.is_dir(),
        };
        if !expected_matches {
            return Err(anyhow!(
                "{label} is not a {}: {}",
                expected.as_str(),
                canonical.display()
            ));
        }
        enforce_roots(&canonical, label, roots)?;
        Ok(canonical)
    }

    fn resolve_artifact_path(&self, path: &Path, label: &str) -> Result<PathBuf> {
        validate_path_syntax(path, label, self.allow_unc_paths)?;
        if path.is_absolute() {
            Ok(path.to_path_buf())
        } else {
            Ok(self.captures_dir.join(path))
        }
    }

    fn validate_output(&self, path: &Path, label: &str) -> Result<PathBuf> {
        validate_path_syntax(path, label, self.allow_unc_paths)?;
        let file_name = path
            .file_name()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| anyhow!("{label} must name a file"))?;

        // Replacing a link is platform-dependent and makes the policy target
        // ambiguous. Regular existing files remain valid for tools that
        // explicitly implement replacement semantics.
        if let Ok(metadata) = std::fs::symlink_metadata(path)
            && metadata.file_type().is_symlink()
        {
            return Err(anyhow!(
                "{label} must not be a symbolic link or reparse-point link: {}",
                path.display()
            ));
        }

        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let canonical_parent = self.validate_existing(
            parent,
            &format!("{label} parent"),
            ExpectedPath::Directory,
            self.output_roots.as_deref(),
        )?;
        let canonical = canonical_parent.join(file_name);
        validate_path_syntax(&canonical, label, self.allow_unc_paths)?;
        enforce_roots(&canonical, label, self.output_roots.as_deref())?;
        Ok(canonical)
    }
}

#[derive(Clone, Copy)]
enum ExpectedPath {
    File,
    Directory,
}

impl ExpectedPath {
    fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
        }
    }
}

/// Validate all environment-backed policy settings early during startup.
pub fn initialize() -> Result<&'static SecurityPolicy> {
    if let Some(policy) = POLICY.get() {
        return Ok(policy);
    }
    let policy = SecurityPolicy::from_env()?;
    // Another thread may have initialized the identical process-wide policy.
    let _ = POLICY.set(policy);
    POLICY
        .get()
        .ok_or_else(|| anyhow!("Failed to initialize the security policy"))
}

fn policy() -> Result<&'static SecurityPolicy> {
    initialize()
}

pub fn capture_directory() -> Result<PathBuf> {
    Ok(policy()?.captures_dir.clone())
}

pub fn max_concurrent_tools() -> Result<usize> {
    Ok(policy()?.max_concurrent_tools)
}

pub fn elevated_launch_allowed() -> Result<bool> {
    Ok(policy()?.allow_elevated_launch)
}

pub fn validate_input_file(path: &Path, label: &str) -> Result<PathBuf> {
    let policy = policy()?;
    let path = policy.resolve_artifact_path(path, label)?;
    policy.validate_existing(
        &path,
        label,
        ExpectedPath::File,
        policy.input_roots.as_deref(),
    )
}

pub fn validate_input_directory(path: &Path, label: &str) -> Result<PathBuf> {
    let policy = policy()?;
    let path = policy.resolve_artifact_path(path, label)?;
    policy.validate_existing(
        &path,
        label,
        ExpectedPath::Directory,
        policy.input_roots.as_deref(),
    )
}

pub fn validate_output_file(path: &Path, label: &str) -> Result<PathBuf> {
    let policy = policy()?;
    let path = policy.resolve_artifact_path(path, label)?;
    policy.validate_output(&path, label)
}

pub fn resolve_artifact_path(path: &Path, label: &str) -> Result<PathBuf> {
    policy()?.resolve_artifact_path(path, label)
}

pub fn validate_executable(path: &Path) -> Result<PathBuf> {
    let policy = policy()?;
    policy.validate_existing(
        path,
        "Executable path",
        ExpectedPath::File,
        policy.executable_roots.as_deref(),
    )
}

pub fn validate_working_directory(path: &Path) -> Result<PathBuf> {
    let policy = policy()?;
    policy.validate_existing(
        path,
        "Working directory",
        ExpectedPath::Directory,
        policy.executable_roots.as_deref(),
    )
}

/// Block user-controlled application launches from an elevated server unless
/// the operator explicitly accepts that privilege boundary.
pub fn ensure_user_launch_allowed() -> Result<()> {
    enforce_elevated_launch_policy(is_elevated()?, policy()?.allow_elevated_launch)
}

fn enforce_elevated_launch_policy(is_elevated: bool, allow_elevated_launch: bool) -> Result<()> {
    if is_elevated && !allow_elevated_launch {
        return Err(anyhow!(
            "Application-launch tools are disabled while pix-mcp is elevated. Run a separate \
             non-elevated server for launch/GPU-capture workflows, or set \
             PIX_MCP_ALLOW_ELEVATED_LAUNCH=true only for trusted executables. Timing capture by \
             PID remains available while elevated."
        ));
    }
    Ok(())
}

fn parse_bool_setting(name: &str, raw: Option<String>, default: bool) -> Result<bool> {
    let Some(raw) = raw else {
        return Ok(default);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(anyhow!("{name} must be true/false, 1/0, yes/no, or on/off")),
    }
}

fn parse_concurrency(raw: Option<String>) -> Result<usize> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_MAX_CONCURRENT_TOOLS);
    };
    let value = raw
        .trim()
        .parse::<usize>()
        .map_err(|_| anyhow!("PIX_MCP_MAX_CONCURRENT_TOOLS must be an integer between 1 and 64"))?;
    if !(1..=MAX_CONFIGURED_CONCURRENT_TOOLS).contains(&value) {
        return Err(anyhow!(
            "PIX_MCP_MAX_CONCURRENT_TOOLS must be between 1 and 64"
        ));
    }
    Ok(value)
}

fn parse_roots(
    name: &str,
    raw: Option<String>,
    implicit_root: Option<&Path>,
    allow_unc_paths: bool,
) -> Result<Option<Vec<PathBuf>>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let mut roots = Vec::new();
    if let Some(root) = implicit_root {
        roots.push(root.to_path_buf());
    }
    for entry in raw
        .split(';')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        roots.push(canonical_directory(
            Path::new(entry),
            name,
            allow_unc_paths,
        )?);
    }
    roots.sort();
    roots.dedup();
    if roots.is_empty() {
        return Err(anyhow!(
            "{name} must contain at least one existing directory"
        ));
    }
    Ok(Some(roots))
}

fn canonical_directory(path: &Path, label: &str, allow_unc_paths: bool) -> Result<PathBuf> {
    validate_path_syntax(path, label, allow_unc_paths)?;
    let canonical = canonicalize(path)
        .with_context(|| format!("{label} directory does not exist: {}", path.display()))?;
    validate_path_syntax(&canonical, label, allow_unc_paths)?;
    if !canonical.is_dir() {
        return Err(anyhow!(
            "{label} is not a directory: {}",
            canonical.display()
        ));
    }
    Ok(canonical)
}

fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)?;
    Ok(remove_verbatim_prefix(canonical))
}

#[cfg(windows)]
fn remove_verbatim_prefix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

#[cfg(not(windows))]
fn remove_verbatim_prefix(path: PathBuf) -> PathBuf {
    path
}

fn enforce_roots(path: &Path, label: &str, roots: Option<&[PathBuf]>) -> Result<()> {
    let Some(roots) = roots else {
        return Ok(());
    };
    if roots.iter().any(|root| path.starts_with(root)) {
        return Ok(());
    }
    Err(anyhow!(
        "{label} is outside the configured allowlist ({} root{} configured)",
        roots.len(),
        if roots.len() == 1 { "" } else { "s" }
    ))
}

fn validate_path_syntax(path: &Path, label: &str, allow_unc_paths: bool) -> Result<()> {
    if path.as_os_str().is_empty() || path.to_string_lossy().trim().is_empty() {
        return Err(anyhow!("{label} must not be empty"));
    }

    #[cfg(windows)]
    validate_windows_path(path, label, allow_unc_paths)?;

    #[cfg(not(windows))]
    let _ = allow_unc_paths;

    Ok(())
}

#[cfg(windows)]
fn validate_windows_path(path: &Path, label: &str, allow_unc_paths: bool) -> Result<()> {
    use std::path::Prefix;

    let text = path.to_string_lossy();
    let normalized = text.replace('/', "\\");
    let upper = normalized.to_ascii_uppercase();
    if upper.starts_with(r"\\?\")
        || upper.starts_with(r"\\.\")
        || upper.starts_with(r"\??\")
        || upper.starts_with(r"\\GLOBALROOT\")
    {
        return Err(anyhow!(
            "{label} uses a Windows device/verbatim namespace, which is not allowed"
        ));
    }

    if let Some(Component::Prefix(prefix)) = path.components().next() {
        match prefix.kind() {
            Prefix::UNC(_, _) if !allow_unc_paths => {
                return Err(anyhow!(
                    "{label} is a UNC path; set PIX_MCP_ALLOW_UNC_PATHS=true to opt in"
                ));
            }
            Prefix::Verbatim(_)
            | Prefix::VerbatimUNC(_, _)
            | Prefix::VerbatimDisk(_)
            | Prefix::DeviceNS(_) => {
                return Err(anyhow!(
                    "{label} uses a Windows device/verbatim namespace, which is not allowed"
                ));
            }
            Prefix::Disk(_) if !path.is_absolute() => {
                return Err(anyhow!(
                    "{label} uses a drive-relative path; use an absolute path or a normal relative path"
                ));
            }
            _ => {}
        }
    }
    if normalized.starts_with(r"\\") && !allow_unc_paths {
        return Err(anyhow!(
            "{label} is a UNC path; set PIX_MCP_ALLOW_UNC_PATHS=true to opt in"
        ));
    }

    for component in path.components() {
        let Component::Normal(component) = component else {
            continue;
        };
        let component = component.to_string_lossy();
        if component.ends_with([' ', '.']) {
            return Err(anyhow!(
                "{label} contains a component ending in a space or dot, which is ambiguous on Windows"
            ));
        }
        if component.contains(':') {
            return Err(anyhow!(
                "{label} contains an alternate-data-stream separator, which is not allowed"
            ));
        }
        let stem = component
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        let numbered_device = ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        });
        if matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
        ) || numbered_device
        {
            return Err(anyhow!(
                "{label} contains a reserved Windows device name: {component}"
            ));
        }
    }
    Ok(())
}

/// Remove likely secrets, Windows paths, unsafe controls, and excessive data
/// from subprocess diagnostics before they can reach an MCP client or logs.
pub fn sanitize_process_output(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let lower: Vec<char> = input.to_ascii_lowercase().chars().collect();
    let secret_markers = [
        "password=",
        "passwd=",
        "token=",
        "secret=",
        "api_key=",
        "apikey=",
        "authorization:",
    ];
    let mut output = String::with_capacity(input.len().min(MAX_PUBLIC_DIAGNOSTIC_BYTES));
    let mut index = 0;

    while index < chars.len() {
        if let Some(marker) = secret_markers.iter().find(|marker| {
            let marker: Vec<char> = marker.chars().collect();
            lower.get(index..index + marker.len()) == Some(marker.as_slice())
        }) {
            output.push_str(marker);
            output.push_str("<redacted>");
            index += marker.chars().count();
            // Secret values may be quoted or use schemes such as
            // `Authorization: Bearer ...`; redact the remainder of the line.
            while index < chars.len() && !matches!(chars[index], '\r' | '\n') {
                index += 1;
            }
        } else if is_windows_path_start(&chars, index) {
            output.push_str("<redacted-path>");
            index += if chars.get(index) == Some(&'\\') {
                2
            } else {
                3
            };
            while index < chars.len()
                && !matches!(
                    chars[index],
                    '\r' | '\n' | '"' | '\'' | '<' | '>' | '|' | ';'
                )
            {
                index += 1;
            }
        } else {
            let ch = chars[index];
            if ch.is_control() && !matches!(ch, '\r' | '\n' | '\t') {
                output.push('�');
            } else {
                output.push(ch);
            }
            index += 1;
        }

        if output.len() >= MAX_PUBLIC_DIAGNOSTIC_BYTES {
            let mut cutoff = MAX_PUBLIC_DIAGNOSTIC_BYTES.min(output.len());
            while !output.is_char_boundary(cutoff) {
                cutoff -= 1;
            }
            output.truncate(cutoff);
            output.push_str(DIAGNOSTIC_TRUNCATION_MARKER);
            break;
        }
    }
    output
}

fn is_windows_path_start(chars: &[char], index: usize) -> bool {
    chars.get(index..index + 3).is_some_and(|slice| {
        slice[0].is_ascii_alphabetic() && slice[1] == ':' && matches!(slice[2], '\\' | '/')
    }) || chars
        .get(index..index + 2)
        .is_some_and(|slice| slice == ['\\', '\\'])
}

/// Check if the current process has an elevated Windows token. Callers fail
/// closed if Windows cannot provide a reliable privilege result.
pub fn is_elevated() -> Result<bool> {
    #[cfg(windows)]
    {
        use std::mem;
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::Security::{
            GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
        };
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        unsafe {
            let mut token_handle: HANDLE = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token_handle)
                .context("Could not open the process token to determine elevation")?;
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
            result.context("Could not query process-token elevation")?;
            Ok(elevation.TokenIsElevated != 0)
        }
    }

    #[cfg(not(windows))]
    {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn policy_from(values: &[(&str, String)]) -> Result<SecurityPolicy> {
        let values: HashMap<String, String> = values
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect();
        SecurityPolicy::from_lookup(|name| Ok(values.get(name).cloned()))
    }

    #[test]
    fn allowlists_resolve_paths_and_reject_escapes() {
        let captures = tempfile::tempdir().expect("captures directory");
        let outside = tempfile::tempdir().expect("outside directory");
        let inside_file = captures.path().join("inside.wpix");
        let outside_file = outside.path().join("outside.wpix");
        std::fs::write(&inside_file, b"inside").expect("inside file");
        std::fs::write(&outside_file, b"outside").expect("outside file");

        let policy = policy_from(&[
            (
                "PIX_MCP_CAPTURES_DIR",
                captures.path().to_string_lossy().into_owned(),
            ),
            ("PIX_MCP_INPUT_ROOTS", String::new()),
            ("PIX_MCP_OUTPUT_ROOTS", String::new()),
        ])
        .expect("policy");

        assert!(
            policy
                .validate_existing(
                    &inside_file,
                    "capture",
                    ExpectedPath::File,
                    policy.input_roots.as_deref()
                )
                .is_ok()
        );
        assert!(
            policy
                .validate_existing(
                    &outside_file,
                    "capture",
                    ExpectedPath::File,
                    policy.input_roots.as_deref()
                )
                .is_err()
        );
        assert!(
            policy
                .validate_output(&captures.path().join("new.wpix"), "capture output")
                .is_ok()
        );
        assert!(
            policy
                .validate_output(&outside.path().join("new.wpix"), "capture output")
                .is_err()
        );
        assert_eq!(
            policy
                .resolve_artifact_path(Path::new("relative.wpix"), "capture")
                .expect("relative artifact"),
            policy.captures_dir.join("relative.wpix")
        );
    }

    #[test]
    fn policy_settings_are_strict() {
        assert!(parse_bool_setting("TEST", Some("maybe".into()), false).is_err());
        assert!(parse_concurrency(Some("0".into())).is_err());
        assert!(parse_concurrency(Some("65".into())).is_err());
        assert_eq!(parse_concurrency(Some("4".into())).unwrap(), 4);
        assert!(enforce_elevated_launch_policy(true, false).is_err());
        assert!(enforce_elevated_launch_policy(true, true).is_ok());
        assert!(enforce_elevated_launch_policy(false, false).is_ok());
    }

    #[test]
    fn process_output_is_sanitized_and_bounded() {
        let output = sanitize_process_output(
            "token=hunter2\n\u{0007} opened C:\\Users\\Alice\\secret.wpix\nnext",
        );
        assert!(!output.contains("Alice"));
        assert!(!output.contains("hunter2"));
        assert!(output.contains("<redacted-path>"));
        assert!(output.contains("token=<redacted>"));
        assert!(output.contains('�'));

        let authorization = sanitize_process_output("Authorization: Bearer super-secret\nok");
        assert!(!authorization.contains("Bearer"));
        assert!(!authorization.contains("super-secret"));

        let large = sanitize_process_output(&"x".repeat(MAX_PUBLIC_DIAGNOSTIC_BYTES + 1024));
        assert!(large.ends_with(DIAGNOSTIC_TRUNCATION_MARKER));
    }

    #[cfg(windows)]
    #[test]
    fn windows_ambiguous_and_device_paths_are_rejected() {
        for path in [
            r"\\?\C:\\captures\\a.wpix",
            r"\\.\PhysicalDrive0",
            r"C:relative.wpix",
            r"C:\\captures\\NUL.wpix",
            r"C:\\captures\\CONIN$.txt",
            r"C:\\captures\\CONOUT$.txt",
            r"C:\\captures\\COM¹.txt",
            r"C:\\captures\\file.wpix:stream",
            r"C:\\captures\\trailing.\\a.wpix",
        ] {
            assert!(
                validate_windows_path(Path::new(path), "test path", false).is_err(),
                "accepted {path}"
            );
        }
        assert!(validate_windows_path(Path::new(r"\\server\share\a.wpix"), "test", false).is_err());
        assert!(validate_windows_path(Path::new(r"\\server\share\a.wpix"), "test", true).is_ok());
    }
}
