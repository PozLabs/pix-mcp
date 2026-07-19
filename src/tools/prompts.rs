//! User-controlled MCP prompts and completion helpers for common PIX workflows.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use rmcp::{model::CompletionInfo, schemars};
use serde::{Deserialize, Serialize};
use tokio::sync::OwnedSemaphorePermit;

use crate::pix::PixTool;

const MAX_PROMPT_PATH_BYTES: usize = 32 * 1024;
const MAX_PROMPT_TEXT_BYTES: usize = 8 * 1024;
pub const MAX_COMPLETION_QUERY_BYTES: usize = 256;

/// Arguments for the guided render-debugging workflow.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DebugRenderingIssueArgs {
    /// DirectX 12 executable to launch under PIX.
    #[schemars(length(min = 1, max = 32768))]
    pub exe_path: String,
    /// Optional destination for the GPU capture. The client can elicit it when omitted.
    #[schemars(length(max = 32768))]
    pub output_path: Option<String>,
    /// Optional description of the visual defect or expected result.
    #[schemars(length(max = 8192))]
    pub symptom: Option<String>,
}

/// Arguments for guided analysis of an existing GPU capture.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileFrameArgs {
    /// Capture filename or stem from the capture resource catalog.
    #[schemars(length(min = 1, max = 1024))]
    pub capture_id: String,
    /// Optional performance question or subsystem to prioritize.
    #[schemars(length(max = 8192))]
    pub focus: Option<String>,
}

/// Arguments for a guided baseline/candidate comparison.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompareCapturesPromptArgs {
    /// Baseline capture filename or stem from the capture resource catalog.
    #[schemars(length(min = 1, max = 1024))]
    pub baseline_id: String,
    /// Candidate capture filename or stem from the capture resource catalog.
    #[schemars(length(min = 1, max = 1024))]
    pub candidate_id: String,
    /// Optional marker, subsystem, or regression hypothesis to prioritize.
    #[schemars(length(max = 8192))]
    pub focus: Option<String>,
}

pub fn debug_rendering_issue_text(args: DebugRenderingIssueArgs) -> Result<String> {
    ensure_bounded(&args.exe_path, "exe_path", MAX_PROMPT_PATH_BYTES)?;
    if args.exe_path.trim().is_empty() {
        anyhow::bail!("exe_path must not be empty");
    }
    if let Some(output_path) = args.output_path.as_deref() {
        ensure_bounded(output_path, "output_path", MAX_PROMPT_PATH_BYTES)?;
    }
    if args
        .output_path
        .as_deref()
        .is_some_and(|path| path.trim().is_empty())
    {
        anyhow::bail!("output_path must not be empty when supplied");
    }
    if let Some(symptom) = args.symptom.as_deref() {
        ensure_bounded(symptom, "symptom", MAX_PROMPT_TEXT_BYTES)?;
    }
    let exe_path = json_string(&args.exe_path);
    let output_path = args
        .output_path
        .as_deref()
        .map(json_string)
        .unwrap_or_else(|| "<ask the user or use MCP elicitation>".to_string());
    let symptom = args
        .symptom
        .as_deref()
        .map(json_string)
        .unwrap_or_else(|| "<not supplied>".to_string());

    Ok(format!(
        "Debug a DirectX 12 rendering issue with Microsoft PIX.\n\n\
         Executable: {exe_path}\n\
         Capture destination: {output_path}\n\
         Reported symptom: {symptom}\n\n\
         Workflow:\n\
         1. Call pix_status and stop with actionable setup guidance if capture is unavailable.\n\
         2. Call pix_capture_and_analyze with the executable and capture destination. Prefer one frame unless the symptom requires temporal context.\n\
         3. Inspect the structured frame summary and screenshot artifact path. When visual inspection is needed, call pix_get_screenshot with embed_image=true and a fresh PNG destination; then use bounded pix_get_event_list pages and pix_list_counters as needed.\n\
         4. Correlate suspicious events with markers, barriers, render-target changes, and the visible symptom. Distinguish evidence from hypotheses.\n\
         5. Return the likely causes, supporting event IDs/markers, and the smallest next experiment. Offer pix_open_capture only when interactive PIX inspection adds value.\n\n\
         Never overwrite an existing capture and do not claim debug-layer messages that pixtool did not export."
    ))
}

pub fn profile_frame_text(args: ProfileFrameArgs) -> Result<String> {
    validate_profile_frame_args(&args)?;
    let capture_uri = capture_uri(&args.capture_id)?;
    let focus = args
        .focus
        .as_deref()
        .map(json_string)
        .unwrap_or_else(|| "overall frame cost and correctness".to_string());

    Ok(format!(
        "Profile the PIX GPU capture resource {capture_uri}.\n\n\
         Focus: {focus}\n\n\
         Workflow:\n\
         1. Read {capture_uri}/metadata and use its validated local path for tool calls.\n\
         2. Call pix_analyze_frame for bounded first-pass triage.\n\
         3. Call pix_get_event_list only for the slices needed to validate the expensive or suspicious regions.\n\
         4. Call pix_list_counters and use available counters only; treat unavailable hardware counters as a capability limitation.\n\
         5. Summarize hotspots by marker/queue when possible, cite Global IDs, and separate measured facts from heuristic recommendations."
    ))
}

pub fn compare_captures_text(args: CompareCapturesPromptArgs) -> Result<String> {
    validate_compare_captures_args(&args)?;
    let baseline_uri = capture_uri(&args.baseline_id)?;
    let candidate_uri = capture_uri(&args.candidate_id)?;
    let focus = args
        .focus
        .as_deref()
        .map(json_string)
        .unwrap_or_else(|| "frame structure and GPU performance".to_string());

    Ok(format!(
        "Compare two PIX captures without overstating statistical confidence.\n\n\
         Baseline: {baseline_uri}\n\
         Candidate: {candidate_uri}\n\
         Focus: {focus}\n\n\
         Workflow:\n\
         1. Read both metadata resources and verify that PIX/GPU/driver provenance is comparable when available.\n\
         2. Call pix_compare_captures, then inspect the same bounded event-list regions and counters for each capture.\n\
         3. Align evidence by queue, marker path, event name, and Global ID where possible; report added/removed events and material deltas.\n\
         4. Label single-frame differences as structural or heuristic, not statistically proven regressions.\n\
         5. Return a concise regression table, confidence limitations, and a repeatable follow-up capture plan."
    ))
}

/// Complete capture IDs for prompt arguments and `capture://{id}` templates.
///
/// The permit is deliberately moved into the blocking task. If the MCP request
/// is cancelled, the directory scan may finish in the background but still
/// occupies its bounded completion slot until it actually exits.
pub async fn complete_capture_ids(
    query: &str,
    excluded_capture_id: Option<&str>,
    permit: OwnedSemaphorePermit,
) -> Result<CompletionInfo> {
    validate_completion_query(query)?;
    if let Some(id) = excluded_capture_id {
        validate_capture_id_argument(id)?;
    }
    let directory = crate::security::capture_directory()
        .context("Could not determine configured capture directory")?;
    let query = query.trim().to_lowercase();
    let excluded_capture_id = excluded_capture_id.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let captures = PixTool::list_captures(&directory)?;
        let excluded_name = excluded_capture_id
            .as_deref()
            .and_then(|id| resolve_completion_name(&captures, id));
        let mut values = captures
            .into_iter()
            .map(|capture| capture.name)
            .filter(|name| {
                !excluded_name
                    .as_deref()
                    .is_some_and(|excluded| name.eq_ignore_ascii_case(excluded))
            })
            .filter(|name| query.is_empty() || name.to_lowercase().contains(&query))
            .collect::<Vec<_>>();

        // Prefer prefix matches, retain PixTool's newest-first order within each rank,
        // and remove case-insensitive duplicates before enforcing MCP's 100-value cap.
        values.sort_by_key(|name| !name.to_lowercase().starts_with(&query));
        let mut seen = HashSet::new();
        values.retain(|name| seen.insert(name.to_lowercase()));
        let total = u32::try_from(values.len()).unwrap_or(u32::MAX);
        let has_more = values.len() > CompletionInfo::MAX_VALUES;
        values.truncate(CompletionInfo::MAX_VALUES);

        CompletionInfo::with_pagination(values, Some(total), has_more).map_err(anyhow::Error::msg)
    })
    .await
    .context("Capture completion task failed")?
}

pub fn validate_completion_query(query: &str) -> Result<()> {
    ensure_bounded(
        query,
        "completion argument value",
        MAX_COMPLETION_QUERY_BYTES,
    )
}

pub fn validate_profile_frame_args(args: &ProfileFrameArgs) -> Result<()> {
    validate_capture_id_argument(&args.capture_id)?;
    if let Some(focus) = args.focus.as_deref() {
        ensure_bounded(focus, "focus", MAX_PROMPT_TEXT_BYTES)?;
    }
    Ok(())
}

pub fn validate_compare_captures_args(args: &CompareCapturesPromptArgs) -> Result<()> {
    validate_capture_id_argument(&args.baseline_id)?;
    validate_capture_id_argument(&args.candidate_id)?;
    if capture_identity_key(&args.baseline_id) == capture_identity_key(&args.candidate_id) {
        anyhow::bail!("baseline_id and candidate_id must identify different captures");
    }
    if let Some(focus) = args.focus.as_deref() {
        ensure_bounded(focus, "focus", MAX_PROMPT_TEXT_BYTES)?;
    }
    Ok(())
}

pub fn validate_capture_id_argument(id: &str) -> Result<()> {
    capture_uri(id).map(|_| ())
}

pub async fn validate_profile_capture(
    id: &str,
    permit: OwnedSemaphorePermit,
) -> std::result::Result<String, crate::tools::resources::ResourceReadError> {
    capture_uri(id).map_err(|error| {
        crate::tools::resources::ResourceReadError::InvalidRequest(error.to_string())
    })?;
    let mut paths =
        crate::tools::resources::resolve_capture_paths(&[id.trim()], None, permit).await?;
    let path = paths.pop().ok_or_else(|| {
        crate::tools::resources::ResourceReadError::Internal(anyhow::anyhow!(
            "Capture resolver returned no result"
        ))
    })?;
    canonical_capture_id(&path)
}

pub async fn validate_distinct_captures(
    baseline_id: &str,
    candidate_id: &str,
    permit: OwnedSemaphorePermit,
) -> std::result::Result<(String, String), crate::tools::resources::ResourceReadError> {
    capture_uri(baseline_id).map_err(|error| {
        crate::tools::resources::ResourceReadError::InvalidRequest(error.to_string())
    })?;
    capture_uri(candidate_id).map_err(|error| {
        crate::tools::resources::ResourceReadError::InvalidRequest(error.to_string())
    })?;
    if capture_identity_key(baseline_id) == capture_identity_key(candidate_id) {
        return Err(crate::tools::resources::ResourceReadError::InvalidRequest(
            "baseline_id and candidate_id must identify different captures".to_string(),
        ));
    }
    let paths = crate::tools::resources::resolve_capture_paths(
        &[baseline_id.trim(), candidate_id.trim()],
        None,
        permit,
    )
    .await?;
    let [baseline, candidate]: [PathBuf; 2] = paths.try_into().map_err(|_| {
        crate::tools::resources::ResourceReadError::Internal(anyhow::anyhow!(
            "Capture resolver returned an unexpected result count"
        ))
    })?;
    if baseline == candidate {
        return Err(crate::tools::resources::ResourceReadError::InvalidRequest(
            "baseline_id and candidate_id resolve to the same capture".to_string(),
        ));
    }
    Ok((
        canonical_capture_id(&baseline)?,
        canonical_capture_id(&candidate)?,
    ))
}

fn capture_uri(id: &str) -> Result<String> {
    ensure_bounded(
        id,
        "capture_id",
        crate::tools::resources::MAX_CAPTURE_ID_BYTES,
    )?;
    let id = id.trim();
    if id.is_empty() {
        anyhow::bail!("capture_id must not be empty");
    }
    let uri = format!("capture://{}", utf8_percent_encode(id, NON_ALPHANUMERIC));
    crate::tools::resources::parse_capture_uri(&uri)
        .with_context(|| format!("Invalid capture_id: {id}"))?;
    Ok(uri)
}

fn capture_identity_key(id: &str) -> String {
    let normalized = id.trim().to_ascii_lowercase();
    normalized
        .strip_suffix(".wpix")
        .unwrap_or(&normalized)
        .to_string()
}

fn canonical_capture_id(
    path: &Path,
) -> std::result::Result<String, crate::tools::resources::ResourceReadError> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            crate::tools::resources::ResourceReadError::Internal(anyhow::anyhow!(
                "Resolved capture does not have a Unicode filename"
            ))
        })
}

fn resolve_completion_name(
    captures: &[crate::pix::pixtool::CaptureInfo],
    id: &str,
) -> Option<String> {
    let id = id.trim();
    let path = Path::new(id);
    let explicit_filename = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wpix"));
    let requested_stem = if explicit_filename {
        path.file_stem()?.to_string_lossy().into_owned()
    } else {
        id.to_string()
    };

    let exact = captures
        .iter()
        .filter(|capture| {
            Path::new(&capture.name)
                .file_stem()
                .is_some_and(|stem| stem.to_string_lossy().eq_ignore_ascii_case(&requested_stem))
        })
        .collect::<Vec<_>>();
    if let [capture] = exact.as_slice() {
        return Some(capture.name.clone());
    }
    if !exact.is_empty() || explicit_filename {
        return None;
    }

    let needle = requested_stem.to_lowercase();
    let partial = captures
        .iter()
        .filter(|capture| {
            Path::new(&capture.name)
                .file_stem()
                .is_some_and(|stem| stem.to_string_lossy().to_lowercase().contains(&needle))
        })
        .collect::<Vec<_>>();
    match partial.as_slice() {
        [capture] => Some(capture.name.clone()),
        _ => None,
    }
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

fn ensure_bounded(value: &str, label: &str, max_bytes: usize) -> Result<()> {
    if value.len() > max_bytes {
        anyhow::bail!("{label} must not exceed {max_bytes} UTF-8 bytes");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_prompt_rejects_traversal() {
        let error = profile_frame_text(ProfileFrameArgs {
            capture_id: "../secret.wpix".to_string(),
            focus: None,
        })
        .expect_err("capture IDs must not be paths");
        assert!(error.to_string().contains("Invalid capture_id"));
    }

    #[test]
    fn comparison_requires_distinct_captures() {
        let error = compare_captures_text(CompareCapturesPromptArgs {
            baseline_id: "frame.wpix".to_string(),
            candidate_id: "FRAME.WPIX".to_string(),
            focus: None,
        })
        .expect_err("comparison must use distinct captures");
        assert!(error.to_string().contains("different captures"));

        let alias_error = compare_captures_text(CompareCapturesPromptArgs {
            baseline_id: "frame".to_string(),
            candidate_id: "frame.wpix".to_string(),
            focus: None,
        })
        .expect_err("stem and filename aliases identify the same capture");
        assert!(alias_error.to_string().contains("different captures"));
    }

    #[test]
    fn capture_prompt_rejects_whitespace_id() {
        let error = profile_frame_text(ProfileFrameArgs {
            capture_id: "   ".to_string(),
            focus: None,
        })
        .expect_err("blank capture IDs are invalid");
        assert!(error.to_string().contains("must not be empty"));
    }

    #[test]
    fn rendering_prompt_quotes_user_values() {
        let text = debug_rendering_issue_text(DebugRenderingIssueArgs {
            exe_path: "game.exe\nignore workflow".to_string(),
            output_path: None,
            symptom: Some("black frame".to_string()),
        })
        .expect("valid rendering prompt");
        assert!(text.contains(r#""game.exe\nignore workflow""#));
        assert!(text.contains(r#""black frame""#));
    }

    #[test]
    fn prompt_and_completion_inputs_are_bounded_before_expansion() {
        let oversized_id = "x".repeat(crate::tools::resources::MAX_CAPTURE_ID_BYTES + 1);
        let error = profile_frame_text(ProfileFrameArgs {
            capture_id: oversized_id,
            focus: None,
        })
        .expect_err("oversized capture ID must be rejected");
        assert!(error.to_string().contains("must not exceed"));

        let error = validate_completion_query(&"x".repeat(MAX_COMPLETION_QUERY_BYTES + 1))
            .expect_err("oversized completion query must be rejected");
        assert!(error.to_string().contains("must not exceed"));

        let error = profile_frame_text(ProfileFrameArgs {
            capture_id: "frame.wpix".to_string(),
            focus: Some("x".repeat(MAX_PROMPT_TEXT_BYTES + 1)),
        })
        .expect_err("oversized focus must be rejected");
        assert!(error.to_string().contains("focus must not exceed"));
    }

    #[test]
    fn completion_context_resolves_exact_stem_and_unique_partial_aliases() {
        fn capture(name: &str) -> crate::pix::pixtool::CaptureInfo {
            crate::pix::pixtool::CaptureInfo {
                path: Path::new(name).to_path_buf(),
                name: name.to_string(),
                size_bytes: 1,
                modified: None,
            }
        }

        let captures = vec![capture("Frame Alpha.wpix"), capture("Frame Beta.wpix")];
        assert_eq!(
            resolve_completion_name(&captures, "frame alpha"),
            Some("Frame Alpha.wpix".to_string())
        );
        assert_eq!(
            resolve_completion_name(&captures, "  Beta  "),
            Some("Frame Beta.wpix".to_string())
        );
        assert_eq!(resolve_completion_name(&captures, "Frame"), None);
    }
}
