//! MCP resource logic for PIX captures (`capture://...` URIs).

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::json;

/// A parsed `capture://` resource URI.
#[derive(Debug, Clone)]
pub enum CaptureResource {
    List,
    Capture(String),
    Events(String),
    Counters(String),
    Metadata(String),
}

/// Parse a `capture://` resource URI.
pub fn parse_capture_uri(uri: &str) -> Option<CaptureResource> {
    let path = uri.strip_prefix("capture://")?;

    if path == "list" || path.is_empty() {
        return Some(CaptureResource::List);
    }

    let parts: Vec<&str> = path.split('/').collect();
    match parts.as_slice() {
        [id] => Some(CaptureResource::Capture(id.to_string())),
        [id, "events"] => Some(CaptureResource::Events(id.to_string())),
        [id, "counters"] => Some(CaptureResource::Counters(id.to_string())),
        [id, "metadata"] => Some(CaptureResource::Metadata(id.to_string())),
        _ => None,
    }
}

/// Read a `capture://` resource and return its JSON text payload.
pub async fn read_capture_resource_text(uri: &str, captures_dir: Option<&Path>) -> Result<String> {
    let resource =
        parse_capture_uri(uri).ok_or_else(|| anyhow::anyhow!("Invalid capture URI: {}", uri))?;

    let dir = captures_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let payload = match resource {
        CaptureResource::List => {
            let captures = crate::pix::PixTool::list_captures(&dir)?;
            let list: Vec<_> = captures.iter().map(|c| c.to_json()).collect();
            json!({
                "captures": list,
                "count": captures.len(),
                "directory": dir.to_string_lossy()
            })
        }
        CaptureResource::Capture(id) | CaptureResource::Metadata(id) => {
            let capture_path = find_capture_by_id(&dir, &id)?;
            let metadata = std::fs::metadata(&capture_path)?;
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
        CaptureResource::Events(_id) => json!({
            "note": "Use the pix_get_event_list tool to extract events from a capture.",
        }),
        CaptureResource::Counters(_id) => json!({
            "note": "Use the pix_list_counters tool to list counters for a capture.",
        }),
    };

    Ok(serde_json::to_string_pretty(&payload)?)
}

/// Find a capture file by ID (filename stem, full path, or partial match).
fn find_capture_by_id(dir: &Path, id: &str) -> Result<PathBuf> {
    let as_path = PathBuf::from(id);
    if as_path.exists() && as_path.extension().is_some_and(|e| e == "wpix") {
        return Ok(as_path);
    }

    let with_ext = PathBuf::from(format!("{}.wpix", id));
    if with_ext.exists() {
        return Ok(with_ext);
    }

    let in_dir = dir.join(format!("{}.wpix", id));
    if in_dir.exists() {
        return Ok(in_dir);
    }

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "wpix") {
            if let Some(stem) = path.file_stem() {
                if stem.to_string_lossy().contains(id) {
                    return Ok(path);
                }
            }
        }
    }

    Err(anyhow::anyhow!("Capture not found: {}", id))
}
