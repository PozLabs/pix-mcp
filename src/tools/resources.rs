//! MCP resource logic for PIX captures (`capture://...` URIs).

use std::{
    collections::{HashMap, VecDeque},
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use rmcp::model::{Annotations, Resource, Role};
use serde_json::json;
use tokio::sync::OwnedSemaphorePermit;

const MAX_CAPTURE_DIRECTORY_ENTRIES: usize = 20_000;
const MAX_ARTIFACT_REGISTRY_ENTRIES: usize = 4_096;
const MAX_INLINE_ARTIFACT_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_CAPTURE_ID_BYTES: usize = 1_024;
const MAX_AMBIGUOUS_MATCHES_REPORTED: usize = 10;
const MAX_AMBIGUOUS_NAME_BYTES: usize = 256;

/// Failures from `resources/read`, classified so the protocol layer can use
/// the correct JSON-RPC error instead of reporting every failure as missing.
#[derive(Debug)]
pub enum ResourceReadError {
    InvalidRequest(String),
    NotFound(String),
    Internal(anyhow::Error),
}

impl fmt::Display for ResourceReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message) | Self::NotFound(message) => formatter.write_str(message),
            Self::Internal(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ResourceReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Internal(error) => Some(error.as_ref()),
            Self::InvalidRequest(_) | Self::NotFound(_) => None,
        }
    }
}

#[derive(Debug)]
enum CaptureLookupError {
    InvalidId(String),
    NotFound(String),
    Ambiguous(String),
    Internal(anyhow::Error),
}

impl fmt::Display for CaptureLookupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId(message) | Self::NotFound(message) | Self::Ambiguous(message) => {
                formatter.write_str(message)
            }
            Self::Internal(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CaptureLookupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Internal(error) => Some(error.as_ref()),
            Self::InvalidId(_) | Self::NotFound(_) | Self::Ambiguous(_) => None,
        }
    }
}

/// A parsed `capture://` resource URI.
#[derive(Debug, Clone)]
pub enum CaptureResource {
    List,
    Capture(String),
    Events(String),
    Counters(String),
    Metadata(String),
}

fn assistant_annotations(priority: f32) -> Annotations {
    Annotations::default()
        .with_audience(vec![Role::Assistant])
        .with_priority(priority)
}

fn capture_uri(name: &str) -> String {
    format!("capture://{}", utf8_percent_encode(name, NON_ALPHANUMERIC))
}

#[derive(Debug, Clone)]
struct ArtifactEntry {
    path: PathBuf,
    title: String,
    name: String,
    original_mime_type: String,
    descriptor_only: bool,
}

#[derive(Debug, Default)]
struct ArtifactRegistryState {
    next_id: u64,
    order: VecDeque<u64>,
    entries: HashMap<u64, ArtifactEntry>,
}

/// Session-local registry backing dereferenceable `artifact://` resource
/// links. Only artifacts returned by successful tools are registered; clients
/// cannot turn an arbitrary local path into a resource URI.
#[derive(Debug, Clone, Default)]
pub struct ArtifactRegistry {
    inner: Arc<Mutex<ArtifactRegistryState>>,
}

impl ArtifactRegistry {
    fn register(&self, entry: ArtifactEntry) -> Option<u64> {
        let mut state = self.inner.lock().ok()?;
        state.next_id = state.next_id.wrapping_add(1).max(1);
        let id = state.next_id;
        if state.entries.len() >= MAX_ARTIFACT_REGISTRY_ENTRIES
            && let Some(oldest) = state.order.pop_front()
        {
            state.entries.remove(&oldest);
        }
        state.order.push_back(id);
        state.entries.insert(id, entry);
        Some(id)
    }

    fn get(&self, id: u64) -> std::result::Result<ArtifactEntry, ResourceReadError> {
        let state = self.inner.lock().map_err(|_| {
            ResourceReadError::Internal(anyhow::anyhow!("Artifact registry lock is poisoned"))
        })?;
        state.entries.get(&id).cloned().ok_or_else(|| {
            ResourceReadError::NotFound(format!("Artifact resource not found or expired: {id}"))
        })
    }
}

/// Contents returned by a registered artifact resource.
pub enum ArtifactPayload {
    Text { text: String, mime_type: String },
    Blob { base64: String, mime_type: String },
}

/// Register a successful local output as an MCP resource link. Small text and
/// image artifacts are readable directly; large/binary captures resolve to a
/// bounded JSON descriptor instead of injecting an unbounded blob into MCP.
pub fn local_artifact_resource(
    registry: &ArtifactRegistry,
    path: impl AsRef<Path>,
    title: &str,
    mime_type: &str,
) -> Option<Resource> {
    let path = path.as_ref();
    let resolved = crate::security::resolve_artifact_path(path, "Artifact path").ok()?;
    let canonical = std::fs::canonicalize(resolved).ok()?;
    let metadata = std::fs::metadata(&canonical).ok()?;
    if !metadata.is_file() {
        return None;
    }

    let name = canonical
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| title.to_string());
    let descriptor_only = metadata.len() > MAX_INLINE_ARTIFACT_BYTES
        || mime_type.eq_ignore_ascii_case("application/octet-stream");
    let id = registry.register(ArtifactEntry {
        path: canonical,
        title: title.to_string(),
        name: name.clone(),
        original_mime_type: mime_type.to_string(),
        descriptor_only,
    })?;
    let uri = format!("artifact://local/{id}");
    let exposed_mime_type = if descriptor_only {
        "application/json"
    } else {
        mime_type
    };
    let mut resource = Resource::new(uri, name)
        .with_title(title)
        .with_description(if descriptor_only {
            format!("Bounded descriptor for a local {mime_type} PIX artifact")
        } else {
            "Session-local PIX artifact produced by this server".to_string()
        })
        .with_mime_type(exposed_mime_type)
        .with_annotations(assistant_annotations(0.9));

    if !descriptor_only {
        resource = resource.with_size(metadata.len());
    }
    if let Ok(modified) = metadata.modified() {
        let annotations = assistant_annotations(0.9).with_timestamp(modified.into());
        resource = resource.with_annotations(annotations);
    }
    Some(resource)
}

pub async fn read_artifact_resource(
    registry: &ArtifactRegistry,
    uri: &str,
) -> std::result::Result<ArtifactPayload, ResourceReadError> {
    let id = uri
        .strip_prefix("artifact://local/")
        .filter(|value| !value.is_empty() && !value.contains('/'))
        .ok_or_else(|| ResourceReadError::InvalidRequest("Invalid artifact URI".to_string()))?
        .parse::<u64>()
        .map_err(|_| ResourceReadError::InvalidRequest("Invalid artifact ID".to_string()))?;
    let entry = registry.get(id)?;

    tokio::task::spawn_blocking(move || read_artifact_entry(entry))
        .await
        .map_err(|error| {
            ResourceReadError::Internal(anyhow::anyhow!("Artifact read task failed: {error}"))
        })?
}

fn read_artifact_entry(
    entry: ArtifactEntry,
) -> std::result::Result<ArtifactPayload, ResourceReadError> {
    let canonical = std::fs::canonicalize(&entry.path).map_err(|error| {
        ResourceReadError::Internal(anyhow::Error::new(error).context("Artifact is unavailable"))
    })?;
    if canonical != entry.path {
        return Err(ResourceReadError::InvalidRequest(
            "Artifact path changed after it was registered".to_string(),
        ));
    }
    let metadata = std::fs::metadata(&canonical).map_err(|error| {
        ResourceReadError::Internal(anyhow::Error::new(error).context("Cannot inspect artifact"))
    })?;
    if !metadata.is_file() {
        return Err(ResourceReadError::NotFound(
            "Registered artifact is no longer a file".to_string(),
        ));
    }

    if entry.descriptor_only {
        let descriptor = json!({
            "name": entry.name,
            "title": entry.title,
            "path": canonical.to_string_lossy(),
            "mime_type": entry.original_mime_type,
            "size_bytes": metadata.len(),
            "last_modified": metadata.modified().ok().and_then(|time| {
                time.duration_since(std::time::UNIX_EPOCH).ok().map(|duration| duration.as_secs())
            }),
            "note": "The artifact is intentionally represented by a bounded descriptor; use the local path with an appropriate PIX tool."
        });
        return serde_json::to_string_pretty(&descriptor)
            .map(|text| ArtifactPayload::Text {
                text,
                mime_type: "application/json".to_string(),
            })
            .map_err(|error| ResourceReadError::Internal(error.into()));
    }
    if metadata.len() > MAX_INLINE_ARTIFACT_BYTES {
        return Err(ResourceReadError::InvalidRequest(format!(
            "Artifact grew beyond the {} byte MCP resource limit",
            MAX_INLINE_ARTIFACT_BYTES
        )));
    }

    let bytes = std::fs::read(&canonical).map_err(|error| {
        ResourceReadError::Internal(anyhow::Error::new(error).context("Could not read artifact"))
    })?;
    if entry.original_mime_type.starts_with("text/")
        || entry.original_mime_type == "application/json"
    {
        let text = String::from_utf8(bytes).map_err(|_| {
            ResourceReadError::InvalidRequest(
                "Registered text artifact is not valid UTF-8".to_string(),
            )
        })?;
        Ok(ArtifactPayload::Text {
            text,
            mime_type: entry.original_mime_type,
        })
    } else {
        Ok(ArtifactPayload::Blob {
            base64: BASE64_STANDARD.encode(bytes),
            mime_type: entry.original_mime_type,
        })
    }
}

/// Build the concrete capture resource catalog. Capture order is
/// deterministic (newest first, then filename) because `PixTool` applies a
/// stable tie-breaker.
pub async fn list_capture_resources(captures_dir: Option<&Path>) -> Result<Vec<Resource>> {
    let dir = match captures_dir {
        Some(dir) => dir.to_path_buf(),
        None => crate::security::capture_directory()?,
    };
    let captures = tokio::task::spawn_blocking(move || crate::pix::PixTool::list_captures(&dir))
        .await
        .context("Capture resource catalog task failed")??;

    let mut resources = Vec::with_capacity(captures.len().saturating_add(1));
    resources.push(
        Resource::new("capture://list", "pix-capture-list")
            .with_title("PIX capture list")
            .with_description("JSON index of PIX captures in the configured capture directory")
            .with_mime_type("application/json")
            .with_annotations(assistant_annotations(1.0)),
    );
    resources.extend(captures.into_iter().map(|capture| {
        let mut annotations = assistant_annotations(0.8);
        if let Some(modified) = capture.modified {
            annotations = annotations.with_timestamp(modified.into());
        }
        Resource::new(capture_uri(&capture.name), capture.name.clone())
            .with_title(capture.name.clone())
            .with_description("Microsoft PIX capture metadata and analysis entry point")
            .with_mime_type("application/json")
            .with_size(capture.size_bytes)
            .with_annotations(annotations)
    }));
    Ok(resources)
}

/// Parse a `capture://` resource URI.
pub fn parse_capture_uri(uri: &str) -> Result<CaptureResource> {
    let path = uri
        .strip_prefix("capture://")
        .ok_or_else(|| anyhow::anyhow!("Capture URI must start with capture://"))?;
    let parts = path
        .split('/')
        .map(decode_uri_segment)
        .collect::<Result<Vec<_>>>()?;

    match parts.as_slice() {
        [segment] if segment.is_empty() || segment == "list" => Ok(CaptureResource::List),
        [id] => {
            validate_capture_id(id)?;
            Ok(CaptureResource::Capture(id.clone()))
        }
        [id, resource] if matches!(resource.as_str(), "events" | "counters" | "metadata") => {
            validate_capture_id(id)?;
            match resource.as_str() {
                "events" => Ok(CaptureResource::Events(id.clone())),
                "counters" => Ok(CaptureResource::Counters(id.clone())),
                "metadata" => Ok(CaptureResource::Metadata(id.clone())),
                _ => unreachable!("resource suffix was checked above"),
            }
        }
        _ => Err(anyhow::anyhow!("Unsupported capture resource URI path")),
    }
}

fn decode_uri_segment(segment: &str) -> Result<String> {
    let bytes = segment.as_bytes();
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes[offset] == b'%' {
            let valid_escape = bytes
                .get(offset + 1..offset + 3)
                .is_some_and(|escape| escape.iter().all(u8::is_ascii_hexdigit));
            if !valid_escape {
                return Err(anyhow::anyhow!(
                    "Invalid percent-encoding in capture URI segment: {}",
                    segment
                ));
            }
            offset += 3;
        } else {
            offset += 1;
        }
    }

    percent_decode_str(segment)
        .decode_utf8()
        .map(|decoded| decoded.into_owned())
        .map_err(|_| {
            anyhow::anyhow!(
                "Percent-encoded capture URI segment is not valid UTF-8: {}",
                segment
            )
        })
}

/// Read a `capture://` resource and return its JSON text payload.
pub async fn read_capture_resource_text(
    uri: &str,
    captures_dir: Option<&Path>,
) -> std::result::Result<String, ResourceReadError> {
    let resource = parse_capture_uri(uri)
        .map_err(|error| ResourceReadError::InvalidRequest(error.to_string()))?;
    let captures_dir = captures_dir.map(Path::to_path_buf);
    tokio::task::spawn_blocking(move || {
        read_capture_resource_text_sync(resource, captures_dir.as_deref())
    })
    .await
    .map_err(|error| {
        ResourceReadError::Internal(anyhow::anyhow!("Capture resource task failed: {error}"))
    })?
}

/// Resolve capture IDs through the same bounded, traversal-safe catalog used
/// by `resources/read`. The catalog permit is held inside the blocking task so
/// cancellation cannot release capacity while the filesystem scan still runs.
pub async fn resolve_capture_paths(
    ids: &[&str],
    captures_dir: Option<&Path>,
    permit: OwnedSemaphorePermit,
) -> std::result::Result<Vec<PathBuf>, ResourceReadError> {
    for id in ids {
        validate_capture_id(id)
            .map_err(|error| ResourceReadError::InvalidRequest(error.to_string()))?;
    }
    let dir = match captures_dir {
        Some(dir) => dir.to_path_buf(),
        None => crate::security::capture_directory().map_err(ResourceReadError::Internal)?,
    };
    let ids = ids.iter().map(|id| (*id).to_string()).collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        // Revalidate and canonicalize immediately before the scan so a capture
        // directory replaced after server startup cannot bypass input roots.
        let dir = crate::security::validate_input_directory(&dir, "Capture directory")
            .map_err(ResourceReadError::Internal)?;
        ids.iter()
            .map(|id| find_capture_by_id(&dir, id).map_err(map_lookup_error))
            .collect()
    })
    .await
    .map_err(|error| {
        ResourceReadError::Internal(anyhow::anyhow!("Capture resolution task failed: {error}"))
    })?
}

fn read_capture_resource_text_sync(
    resource: CaptureResource,
    captures_dir: Option<&Path>,
) -> std::result::Result<String, ResourceReadError> {
    let dir = match captures_dir {
        Some(dir) => dir.to_path_buf(),
        None => crate::security::capture_directory().map_err(ResourceReadError::Internal)?,
    };
    let dir = crate::security::validate_input_directory(&dir, "Capture directory")
        .map_err(ResourceReadError::Internal)?;

    let payload = match resource {
        CaptureResource::List => {
            let captures =
                crate::pix::PixTool::list_captures(&dir).map_err(ResourceReadError::Internal)?;
            const RESOURCE_CAPTURE_LIMIT: usize = 500;
            let total_count = captures.len();
            let list: Vec<_> = captures
                .iter()
                .take(RESOURCE_CAPTURE_LIMIT)
                .map(|c| c.to_json())
                .collect();
            let returned = list.len();
            json!({
                "captures": list,
                "count": returned,
                "total_count": total_count,
                "truncated": returned < total_count,
                "directory": dir.to_string_lossy()
            })
        }
        CaptureResource::Capture(id) | CaptureResource::Metadata(id) => {
            let capture_path = find_capture_by_id(&dir, &id).map_err(map_lookup_error)?;
            let metadata = std::fs::metadata(&capture_path)
                .map_err(|error| ResourceReadError::Internal(error.into()))?;
            json!({
                "id": id,
                "path": capture_path.to_string_lossy(),
                "name": capture_path.file_name().map(|n| n.to_string_lossy()),
                "size_bytes": metadata.len(),
                "modified": metadata.modified().ok().map(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0)
                }),
                "note": "Open in PIX for detailed analysis, or use pix_analyze_capture"
            })
        }
        CaptureResource::Events(id) => {
            let capture_path = find_capture_by_id(&dir, &id).map_err(map_lookup_error)?;
            json!({
                "id": id,
                "path": capture_path.to_string_lossy(),
                "note": "Use the pix_get_event_list tool with this capture path to extract events."
            })
        }
        CaptureResource::Counters(id) => {
            let capture_path = find_capture_by_id(&dir, &id).map_err(map_lookup_error)?;
            json!({
                "id": id,
                "path": capture_path.to_string_lossy(),
                "note": "Use the pix_list_counters tool with this capture path to list counters."
            })
        }
    };

    serde_json::to_string_pretty(&payload)
        .map_err(|error| ResourceReadError::Internal(error.into()))
}

fn map_lookup_error(error: CaptureLookupError) -> ResourceReadError {
    match error {
        CaptureLookupError::InvalidId(message) | CaptureLookupError::Ambiguous(message) => {
            ResourceReadError::InvalidRequest(message)
        }
        CaptureLookupError::NotFound(message) => ResourceReadError::NotFound(message),
        CaptureLookupError::Internal(error) => ResourceReadError::Internal(error),
    }
}

fn has_wpix_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wpix"))
}

fn validate_capture_id(id: &str) -> Result<()> {
    if id.len() > MAX_CAPTURE_ID_BYTES {
        return Err(anyhow::anyhow!(
            "Capture ID must not exceed {} UTF-8 bytes",
            MAX_CAPTURE_ID_BYTES
        ));
    }
    if id.trim().is_empty() {
        return Err(anyhow::anyhow!("Capture ID must not be empty"));
    }

    let path = Path::new(id);
    if path.is_absolute()
        || id.contains('/')
        || id.contains('\\')
        || id.contains(':')
        || path.components().count() != 1
        || !path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
    {
        return Err(anyhow::anyhow!(
            "Capture ID must be a filename or filename stem, not a path: {}",
            id
        ));
    }
    Ok(())
}

fn deterministic_path_sort(left: &Path, right: &Path) -> std::cmp::Ordering {
    let left = left
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let right = right
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    left.to_lowercase()
        .cmp(&right.to_lowercase())
        .then_with(|| left.cmp(&right))
}

fn require_unique_match(
    id: &str,
    matches: &[PathBuf],
) -> std::result::Result<PathBuf, CaptureLookupError> {
    match matches {
        [path] => Ok(path.clone()),
        [] => Err(CaptureLookupError::NotFound(format!(
            "Capture not found: {id}"
        ))),
        paths => {
            let names = paths
                .iter()
                .take(MAX_AMBIGUOUS_MATCHES_REPORTED)
                .filter_map(|path| path.file_name())
                .map(|name| truncate_utf8(&name.to_string_lossy(), MAX_AMBIGUOUS_NAME_BYTES))
                .collect::<Vec<_>>()
                .join(", ");
            let omitted = paths.len().saturating_sub(MAX_AMBIGUOUS_MATCHES_REPORTED);
            let suffix = if omitted == 0 {
                String::new()
            } else {
                format!(", and {omitted} more")
            };
            Err(CaptureLookupError::Ambiguous(format!(
                "Capture ID is ambiguous: {id}. Matches: {names}{suffix}"
            )))
        }
    }
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

/// Find a capture file by filename/stem or an unambiguous partial stem match.
/// Paths are deliberately forbidden: a `capture://` resource cannot escape its
/// configured capture directory.
fn find_capture_by_id(dir: &Path, id: &str) -> std::result::Result<PathBuf, CaptureLookupError> {
    validate_capture_id(id).map_err(|error| CaptureLookupError::InvalidId(error.to_string()))?;
    let canonical_dir = std::fs::canonicalize(dir).map_err(|error| {
        CaptureLookupError::Internal(anyhow::Error::new(error).context(format!(
            "Capture directory does not exist: {}",
            dir.display()
        )))
    })?;
    if !canonical_dir.is_dir() {
        return Err(CaptureLookupError::Internal(anyhow::anyhow!(
            "Capture directory is not a directory: {}",
            canonical_dir.display()
        )));
    }

    let mut captures = Vec::new();
    let entries = std::fs::read_dir(&canonical_dir)
        .map_err(|error| CaptureLookupError::Internal(error.into()))?;
    for (index, entry) in entries.enumerate() {
        if index >= MAX_CAPTURE_DIRECTORY_ENTRIES {
            return Err(CaptureLookupError::Internal(anyhow::anyhow!(
                "Capture directory contains more than {} entries; choose a narrower directory",
                MAX_CAPTURE_DIRECTORY_ENTRIES
            )));
        }
        let entry = entry.map_err(|error| CaptureLookupError::Internal(error.into()))?;
        if !entry
            .file_type()
            .map_err(|error| CaptureLookupError::Internal(error.into()))?
            .is_file()
        {
            continue;
        }
        let path = entry.path();
        if !has_wpix_extension(&path) {
            continue;
        }
        let path = std::fs::canonicalize(path)
            .map_err(|error| CaptureLookupError::Internal(error.into()))?;
        if path.starts_with(&canonical_dir) {
            captures.push(path);
        }
    }
    captures.sort_by(|left, right| deterministic_path_sort(left, right));

    let id_path = Path::new(id);
    let explicit_filename = has_wpix_extension(id_path);
    let requested_stem = if explicit_filename {
        id_path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| id.to_string())
    } else {
        id.to_string()
    };

    let exact_matches: Vec<PathBuf> = captures
        .iter()
        .filter(|path| {
            path.file_stem()
                .is_some_and(|stem| stem.to_string_lossy().eq_ignore_ascii_case(&requested_stem))
        })
        .cloned()
        .collect();
    if !exact_matches.is_empty() {
        return require_unique_match(id, &exact_matches);
    }
    if explicit_filename {
        return Err(CaptureLookupError::NotFound(format!(
            "Capture not found: {id}"
        )));
    }

    let needle = requested_stem.to_lowercase();
    let partial_matches: Vec<PathBuf> = captures
        .into_iter()
        .filter(|path| {
            path.file_stem()
                .is_some_and(|stem| stem.to_string_lossy().to_lowercase().contains(&needle))
        })
        .collect();
    require_unique_match(id, &partial_matches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn test_directory(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "pix_mcp_resources_{}_{}_{}",
            label,
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).expect("create resource test directory");
        path
    }

    #[test]
    fn capture_id_rejects_paths_and_traversal() {
        let dir = test_directory("traversal");
        assert!(find_capture_by_id(&dir, "..\\outside").is_err());
        assert!(find_capture_by_id(&dir, "../outside").is_err());
        assert!(find_capture_by_id(&dir, &dir.join("outside.wpix").to_string_lossy()).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn capture_id_and_ambiguous_errors_are_bounded() {
        let oversized = "x".repeat(MAX_CAPTURE_ID_BYTES + 1);
        let error = validate_capture_id(&oversized).expect_err("oversized ID must be rejected");
        assert!(error.to_string().contains("must not exceed"));

        let matches = (0..12)
            .map(|index| PathBuf::from(format!("scene-{index:02}.wpix")))
            .collect::<Vec<_>>();
        let error = require_unique_match("scene", &matches)
            .expect_err("multiple captures must remain ambiguous")
            .to_string();
        assert!(error.contains("scene-00.wpix"));
        assert!(error.contains("and 2 more"));
        assert!(!error.contains("scene-11.wpix"));
        assert!(error.len() < 4_096, "ambiguous error must stay bounded");
    }

    #[test]
    fn ambiguous_name_truncation_preserves_utf8_boundaries() {
        let value = "€".repeat(100);
        let truncated = truncate_utf8(&value, 256);
        assert!(truncated.ends_with('…'));
        assert!(truncated.len() <= 259);
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }

    #[test]
    fn capture_uri_decodes_spaces_unicode_and_resource_segments() {
        let resource = parse_capture_uri("capture://My%20Cattura-%E2%82%AC/%6detadata")
            .expect("parse encoded capture URI");
        match resource {
            CaptureResource::Metadata(id) => assert_eq!(id, "My Cattura-€"),
            _ => panic!("expected metadata resource"),
        }
    }

    #[test]
    fn capture_uri_decodes_an_encoded_percent_sign() {
        let resource =
            parse_capture_uri("capture://GPU%20100%25").expect("parse encoded percent sign");
        match resource {
            CaptureResource::Capture(id) => assert_eq!(id, "GPU 100%"),
            _ => panic!("expected capture resource"),
        }
    }

    #[test]
    fn capture_uri_rejects_invalid_percent_encoding_and_utf8() {
        for uri in [
            "capture://capture%",
            "capture://capture%2",
            "capture://capture%GG",
            "capture://capture%FF",
        ] {
            assert!(parse_capture_uri(uri).is_err(), "URI should fail: {uri}");
        }
    }

    #[test]
    fn capture_uri_rejects_percent_encoded_traversal() {
        for uri in [
            "capture://%2E%2E",
            "capture://..%2Foutside",
            "capture://..%5Coutside/metadata",
        ] {
            assert!(parse_capture_uri(uri).is_err(), "URI should fail: {uri}");
        }
    }

    #[test]
    fn capture_lookup_is_case_insensitive_for_wpix_extension() {
        let dir = test_directory("case");
        let capture = dir.join("Frame.WPIX");
        std::fs::write(&capture, b"capture").expect("write capture");

        let found = find_capture_by_id(&dir, "frame").expect("find capture by stem");
        assert_eq!(
            found,
            std::fs::canonicalize(capture).expect("canonical capture")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ambiguous_partial_match_is_sorted_and_rejected() {
        let dir = test_directory("ambiguous");
        std::fs::write(dir.join("scene-z.wpix"), b"z").expect("write z capture");
        std::fs::write(dir.join("scene-a.wpix"), b"a").expect("write a capture");

        let error = find_capture_by_id(&dir, "scene").expect_err("partial match is ambiguous");
        let message = error.to_string();
        assert!(message.contains("scene-a.wpix, scene-z.wpix"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn explicit_capture_filename_does_not_fall_back_to_partial_match() {
        let dir = test_directory("explicit");
        std::fs::write(dir.join("scene-old.wpix"), b"old").expect("write capture");

        assert!(find_capture_by_id(&dir, "scene.wpix").is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn resource_read_errors_preserve_protocol_taxonomy() {
        let dir = test_directory("taxonomy");
        std::fs::write(dir.join("scene-a.wpix"), b"a").expect("write capture");
        std::fs::write(dir.join("scene-b.wpix"), b"b").expect("write capture");

        let invalid = read_capture_resource_text("https://example.test/capture", Some(&dir))
            .await
            .expect_err("unsupported URI is invalid");
        assert!(matches!(invalid, ResourceReadError::InvalidRequest(_)));

        let missing = read_capture_resource_text("capture://missing", Some(&dir))
            .await
            .expect_err("missing capture");
        assert!(matches!(missing, ResourceReadError::NotFound(_)));

        let ambiguous = read_capture_resource_text("capture://scene", Some(&dir))
            .await
            .expect_err("ambiguous capture ID");
        assert!(matches!(ambiguous, ResourceReadError::InvalidRequest(_)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn local_artifact_links_are_registered_and_readable() {
        let dir = test_directory("artifact");
        let artifact = dir.join("events.csv");
        std::fs::write(&artifact, b"Name,Duration\nDraw,1\n").expect("write artifact");

        let registry = ArtifactRegistry::default();
        let resource = local_artifact_resource(&registry, &artifact, "PIX event list", "text/csv")
            .expect("artifact resource");
        assert!(resource.uri.starts_with("artifact://local/"));
        assert_eq!(resource.name, "events.csv");
        assert_eq!(resource.mime_type.as_deref(), Some("text/csv"));
        assert_eq!(resource.size, Some(21));
        let annotations = resource.annotations.expect("artifact annotations");
        assert_eq!(annotations.audience, Some(vec![Role::Assistant]));
        assert_eq!(annotations.priority, Some(0.9));
        assert!(annotations.last_modified.is_some());

        let payload = read_artifact_resource(&registry, &resource.uri)
            .await
            .expect("read registered artifact");
        match payload {
            ArtifactPayload::Text { text, mime_type } => {
                assert_eq!(mime_type, "text/csv");
                assert_eq!(text, "Name,Duration\nDraw,1\n");
            }
            ArtifactPayload::Blob { .. } => panic!("CSV should be a text resource"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn binary_capture_link_resolves_to_bounded_descriptor() {
        let dir = test_directory("capture-artifact");
        let artifact = dir.join("frame.wpix");
        std::fs::write(&artifact, b"capture").expect("write capture artifact");
        let registry = ArtifactRegistry::default();
        let resource = local_artifact_resource(
            &registry,
            &artifact,
            "PIX GPU capture",
            "application/octet-stream",
        )
        .expect("capture resource");
        assert_eq!(resource.mime_type.as_deref(), Some("application/json"));
        assert_eq!(resource.size, None);

        let payload = read_artifact_resource(&registry, &resource.uri)
            .await
            .expect("read capture descriptor");
        match payload {
            ArtifactPayload::Text { text, mime_type } => {
                assert_eq!(mime_type, "application/json");
                assert!(text.contains("frame.wpix"));
                assert!(text.contains("application/octet-stream"));
            }
            ArtifactPayload::Blob { .. } => panic!("capture should use a descriptor"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn resource_catalog_lists_capture_metadata_and_annotations() {
        let dir = test_directory("catalog");
        std::fs::write(dir.join("Frame One.wpix"), b"capture").expect("write capture");

        let resources = list_capture_resources(Some(&dir))
            .await
            .expect("capture catalog");
        assert_eq!(resources.len(), 2);
        assert_eq!(resources[0].uri, "capture://list");
        assert_eq!(resources[1].uri, "capture://Frame%20One%2Ewpix");
        assert_eq!(resources[1].size, Some(7));
        assert_eq!(
            resources[1]
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.audience.clone()),
            Some(vec![Role::Assistant])
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
