//! MCP resource logic for PIX captures (`capture://...` URIs).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use percent_encoding::percent_decode_str;
use serde_json::json;

const MAX_CAPTURE_DIRECTORY_ENTRIES: usize = 20_000;

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
pub async fn read_capture_resource_text(uri: &str, captures_dir: Option<&Path>) -> Result<String> {
    let uri = uri.to_string();
    let captures_dir = captures_dir.map(Path::to_path_buf);
    tokio::task::spawn_blocking(move || {
        read_capture_resource_text_sync(&uri, captures_dir.as_deref())
    })
    .await
    .context("Capture resource task failed")?
}

fn read_capture_resource_text_sync(uri: &str, captures_dir: Option<&Path>) -> Result<String> {
    let resource =
        parse_capture_uri(uri).with_context(|| format!("Invalid capture URI: {}", uri))?;

    let dir = match captures_dir {
        Some(dir) => dir.to_path_buf(),
        None => crate::security::capture_directory()?,
    };
    let dir = crate::security::validate_input_directory(&dir, "Capture directory")?;

    let payload = match resource {
        CaptureResource::List => {
            let captures = crate::pix::PixTool::list_captures(&dir)?;
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
        CaptureResource::Events(id) => {
            let capture_path = find_capture_by_id(&dir, &id)?;
            json!({
                "id": id,
                "path": capture_path.to_string_lossy(),
                "note": "Use the pix_get_event_list tool with this capture path to extract events."
            })
        }
        CaptureResource::Counters(id) => {
            let capture_path = find_capture_by_id(&dir, &id)?;
            json!({
                "id": id,
                "path": capture_path.to_string_lossy(),
                "note": "Use the pix_list_counters tool with this capture path to list counters."
            })
        }
    };

    Ok(serde_json::to_string_pretty(&payload)?)
}

fn has_wpix_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wpix"))
}

fn validate_capture_id(id: &str) -> Result<()> {
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

fn require_unique_match(id: &str, matches: &[PathBuf]) -> Result<PathBuf> {
    match matches {
        [path] => Ok(path.clone()),
        [] => Err(anyhow::anyhow!("Capture not found: {}", id)),
        paths => {
            let names = paths
                .iter()
                .filter_map(|path| path.file_name())
                .map(|name| name.to_string_lossy())
                .collect::<Vec<_>>()
                .join(", ");
            Err(anyhow::anyhow!(
                "Capture ID is ambiguous: {}. Matches: {}",
                id,
                names
            ))
        }
    }
}

/// Find a capture file by filename/stem or an unambiguous partial stem match.
/// Paths are deliberately forbidden: a `capture://` resource cannot escape its
/// configured capture directory.
fn find_capture_by_id(dir: &Path, id: &str) -> Result<PathBuf> {
    validate_capture_id(id)?;
    let canonical_dir = std::fs::canonicalize(dir)
        .with_context(|| format!("Capture directory does not exist: {}", dir.display()))?;
    if !canonical_dir.is_dir() {
        return Err(anyhow::anyhow!(
            "Capture directory is not a directory: {}",
            canonical_dir.display()
        ));
    }

    let mut captures = Vec::new();
    for (index, entry) in std::fs::read_dir(&canonical_dir)?.enumerate() {
        if index >= MAX_CAPTURE_DIRECTORY_ENTRIES {
            return Err(anyhow::anyhow!(
                "Capture directory contains more than {} entries; choose a narrower directory",
                MAX_CAPTURE_DIRECTORY_ENTRIES
            ));
        }
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if !has_wpix_extension(&path) {
            continue;
        }
        let path = std::fs::canonicalize(path)?;
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
        return Err(anyhow::anyhow!("Capture not found: {}", id));
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
}
