//! MCP server exposing Microsoft PIX debugging tools via the official `rmcp` SDK.

mod analysis;
mod capture;
mod launch;
mod resources;
mod status;
mod workflow;

use std::{
    collections::hash_map::DefaultHasher,
    future::Future,
    hash::{Hash, Hasher},
    sync::Arc,
    time::Duration,
};

use anyhow::Result as AnyResult;
use rmcp::{
    ErrorData as McpError, Json, Peer, RoleServer, ServerHandler, elicit_safe,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::*,
    schemars,
    service::{ElicitationMode, PeerRequestOptions, RequestContext, RequestHandle},
    tool, tool_router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const TOOL_QUEUE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

enum ToolPermitError {
    Busy,
    Cancelled,
    Closed,
}

const PROGRESS_NOTIFICATION_INTERVAL: Duration = Duration::from_secs(1);
const PROGRESS_NOTIFICATION_TIMEOUT: Duration = Duration::from_millis(250);
const RESOURCE_PAGE_SIZE: usize = 100;
const RESOURCE_CURSOR_PREFIX: &str = "pix-captures-v2:";

/// The PIX MCP server. Tools are registered through the `#[tool_router]` macro.
#[derive(Clone)]
pub struct PixServer {
    tool_router: ToolRouter<Self>,
    artifact_registry: resources::ArtifactRegistry,
}

/// Service wrapper that normalizes task-augmented tool calls to direct calls.
/// rmcp 2.2 otherwise rejects/enqueues them even when the server correctly
/// omits the Tasks capability, contrary to the MCP fallback requirement.
#[derive(Clone)]
pub struct PixService {
    server: PixServer,
    tool_concurrency: Arc<Semaphore>,
}

impl Default for PixServer {
    fn default() -> Self {
        Self::new()
    }
}

/// Elicitation payload used to ask the client for a missing output path.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(description = "Destination file path")]
struct OutputPathRequest {
    /// Path for the output file (absolute paths are recommended).
    #[schemars(length(min = 1))]
    output_path: String,
}
elicit_safe!(OutputPathRequest);

/// Closed schema for tools that intentionally accept no arguments.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

/// Wrap a logic-handler result for an MCP tool. Success becomes a typed
/// `Json<T>` value (producing an `outputSchema` and `structuredContent` plus a
/// text mirror); failures become `isError` tool content via `Err(String)` so
/// the model can read and self-correct (SEP-1303).
fn done<T>(result: AnyResult<T>) -> Result<Json<T>, String> {
    result.map(Json).map_err(|e| e.to_string())
}

#[derive(Debug)]
struct ProgressGate {
    last_progress: f64,
    last_sent: Option<tokio::time::Instant>,
}

impl Default for ProgressGate {
    fn default() -> Self {
        Self {
            last_progress: -1.0,
            last_sent: None,
        }
    }
}

impl ProgressGate {
    fn admit(&mut self, progress: f64, now: tokio::time::Instant, force: bool) -> bool {
        if !progress.is_finite() || progress <= self.last_progress {
            return false;
        }
        if !force
            && self.last_sent.is_some_and(|last| {
                now.saturating_duration_since(last) < PROGRESS_NOTIFICATION_INTERVAL
            })
        {
            return false;
        }
        self.last_progress = progress;
        self.last_sent = Some(now);
        true
    }
}

struct ProgressReporter {
    peer: Peer<RoleServer>,
    token: ProgressToken,
    progress: f64,
    gate: ProgressGate,
}

impl ProgressReporter {
    fn from_context(context: &RequestContext<RoleServer>) -> Option<Self> {
        Some(Self {
            peer: context.peer.clone(),
            token: context.meta.get_progress_token()?,
            progress: 0.0,
            gate: ProgressGate::default(),
        })
    }

    async fn send(&mut self, message: impl Into<String>, force: bool) {
        let progress = self.progress;
        if !self
            .gate
            .admit(progress, tokio::time::Instant::now(), force)
        {
            return;
        }
        let notification = ProgressNotificationParam::new(self.token.clone(), progress)
            .with_message(message.into());
        match tokio::time::timeout(
            PROGRESS_NOTIFICATION_TIMEOUT,
            self.peer.notify_progress(notification),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::debug!(%error, "Could not deliver optional progress notification");
            }
            Err(_) => {
                tracing::debug!("Optional progress notification timed out");
            }
        }
    }

    async fn advance(&mut self, message: impl Into<String>, force: bool) {
        self.progress += 1.0;
        self.send(message, force).await;
    }
}

/// Run a potentially slow operation while emitting optional, monotonically
/// increasing progress. No total is claimed because PIX subprocess duration
/// is generally unknowable. Notification transport failures are deliberately
/// non-fatal to the underlying tool operation.
async fn with_progress<T>(
    context: &RequestContext<RoleServer>,
    operation: &'static str,
    future: impl Future<Output = T>,
) -> T {
    let Some(mut reporter) = ProgressReporter::from_context(context) else {
        return future.await;
    };

    reporter.send(format!("{operation} started"), true).await;
    tokio::pin!(future);
    let mut interval = tokio::time::interval(PROGRESS_NOTIFICATION_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await;

    loop {
        tokio::select! {
            result = &mut future => {
                reporter.advance(format!("{operation} finished"), true).await;
                return result;
            }
            _ = interval.tick() => {
                reporter.advance(format!("{operation} is still running"), false).await;
            }
        }
    }
}

fn structured_call_result<T: Serialize>(
    result: AnyResult<T>,
    resources: impl FnOnce(&T) -> Vec<Resource>,
) -> Result<CallToolResult, McpError> {
    match result {
        Ok(report) => {
            let structured_content = serde_json::to_value(&report).map_err(|error| {
                McpError::internal_error(format!("Failed to serialize tool result: {error}"), None)
            })?;
            let mut call_result = CallToolResult::structured(structured_content);
            call_result.content.extend(
                resources(&report)
                    .into_iter()
                    .map(ContentBlock::resource_link),
            );
            Ok(call_result)
        }
        Err(error) => Ok(CallToolResult::error(vec![ContentBlock::text(
            error.to_string(),
        )])),
    }
}

fn reject_cursor(request: Option<&PaginatedRequestParams>, endpoint: &str) -> Result<(), McpError> {
    if request.and_then(|params| params.cursor.as_ref()).is_some() {
        Err(McpError::invalid_params(
            format!("{endpoint} does not paginate; cursor must be omitted"),
            None,
        ))
    } else {
        Ok(())
    }
}

fn map_resource_error(error: resources::ResourceReadError, uri: &str) -> McpError {
    match error {
        resources::ResourceReadError::InvalidRequest(message) => {
            McpError::invalid_params(message, Some(serde_json::json!({ "uri": uri })))
        }
        resources::ResourceReadError::NotFound(message) => {
            McpError::resource_not_found(message, Some(serde_json::json!({ "uri": uri })))
        }
        resources::ResourceReadError::Internal(error) => {
            tracing::error!(%error, %uri, "Failed to read PIX resource");
            McpError::internal_error(
                "Failed to read PIX resource",
                Some(serde_json::json!({ "uri": uri })),
            )
        }
    }
}

fn resource_catalog_generation(resources: &[Resource]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for resource in resources {
        resource.uri.hash(&mut hasher);
        resource.name.hash(&mut hasher);
        resource.size.hash(&mut hasher);
        resource
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.last_modified.as_ref())
            .map(ToString::to_string)
            .hash(&mut hasher);
    }
    hasher.finish()
}

fn resource_cursor_offset(
    request: Option<&PaginatedRequestParams>,
    generation: u64,
    total: usize,
) -> Result<usize, McpError> {
    let Some(cursor) = request.and_then(|params| params.cursor.as_deref()) else {
        return Ok(0);
    };
    let Some(payload) = cursor.strip_prefix(RESOURCE_CURSOR_PREFIX) else {
        return Err(McpError::invalid_params(
            "Unrecognized resources/list cursor",
            Some(serde_json::json!({ "cursor": cursor })),
        ));
    };
    let Some((cursor_generation, offset)) = payload.split_once(':') else {
        return Err(McpError::invalid_params(
            "Malformed resources/list cursor",
            Some(serde_json::json!({ "cursor": cursor })),
        ));
    };
    if offset.contains(':') {
        return Err(McpError::invalid_params(
            "Malformed resources/list cursor",
            Some(serde_json::json!({ "cursor": cursor })),
        ));
    }
    let cursor_generation = u64::from_str_radix(cursor_generation, 16).map_err(|_| {
        McpError::invalid_params(
            "Malformed resources/list cursor",
            Some(serde_json::json!({ "cursor": cursor })),
        )
    })?;
    let offset = offset.parse::<usize>().map_err(|_| {
        McpError::invalid_params(
            "Malformed resources/list cursor",
            Some(serde_json::json!({ "cursor": cursor })),
        )
    })?;
    if cursor_generation != generation {
        return Err(McpError::invalid_params(
            "resources/list cursor expired because the capture catalog changed",
            Some(serde_json::json!({ "cursor": cursor })),
        ));
    }
    if offset == 0 || offset % RESOURCE_PAGE_SIZE != 0 || offset >= total {
        return Err(McpError::invalid_params(
            "resources/list cursor was not emitted for this catalog",
            Some(serde_json::json!({ "cursor": cursor })),
        ));
    }
    Ok(offset)
}

/// Build the mixed image/structured response used by `pix_get_screenshot`.
/// The JSON text mirrors `structuredContent` for clients that only consume
/// content blocks.
fn screenshot_call_result(
    registry: &resources::ArtifactRegistry,
    res: analysis::ScreenshotResult,
) -> Result<CallToolResult, McpError> {
    let structured_content = serde_json::to_value(&res.report).map_err(|e| {
        McpError::internal_error(format!("Failed to serialize screenshot report: {e}"), None)
    })?;

    let mut result = CallToolResult::structured(structured_content);
    if let Some(b64) = res.image_b64 {
        result
            .content
            .insert(0, ContentBlock::image(b64, "image/png"));
    }
    if let Some(resource) = resources::local_artifact_resource(
        registry,
        &res.report.output_path,
        "PIX screenshot",
        "image/png",
    ) {
        result.content.push(ContentBlock::resource_link(resource));
    }
    Ok(result)
}

async fn cancel_peer_request(handle: RequestHandle<RoleServer>, reason: &'static str) {
    // Spawn before awaiting so cancellation forwarding survives if the outer
    // tool dispatcher drops this future at the same cancellation boundary.
    let cancellation = tokio::spawn(handle.cancel(Some(reason.to_string())));
    match tokio::time::timeout(std::time::Duration::from_secs(1), cancellation).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(error))) => {
            tracing::warn!(%error, "Failed to forward child request cancellation");
        }
        Ok(Err(error)) => {
            tracing::warn!(%error, "Child request cancellation task failed");
        }
        Err(_) => {
            // Dropping a Tokio JoinHandle detaches the task, so it may still
            // complete once the transport accepts another outbound message.
            tracing::warn!("Timed out while forwarding child request cancellation");
        }
    }
}

/// Resolve an output path, asking the client via elicitation when it is missing.
/// Returns an actionable tool error if no path can be obtained.
async fn resolve_output_path(
    ctx: &RequestContext<RoleServer>,
    provided: Option<String>,
    prompt: &str,
) -> Result<String, String> {
    if let Some(p) = provided {
        if p.trim().is_empty() {
            return Err("output_path must not be empty.".to_string());
        }
        return Ok(p);
    }
    if !ctx
        .peer
        .supported_elicitation_modes()
        .contains(&ElicitationMode::Form)
    {
        return Err(
            "output_path is required; the client does not support form elicitation. Please pass output_path."
                .to_string(),
        );
    }

    let requested_schema = ElicitationSchema::from_type::<OutputPathRequest>()
        .map_err(|error| format!("Could not build the output-path form schema: {error}"))?;
    let request = ServerRequest::ElicitRequest(ElicitRequest::new(
        ElicitRequestParams::FormElicitationParams {
            meta: None,
            message: prompt.to_string(),
            requested_schema,
        },
    ));
    let mut handle = ctx
        .peer
        .send_cancellable_request(request, PeerRequestOptions::no_options())
        .await
        .map_err(|error| format!("Could not request output_path from the client: {error}"))?;

    let response = tokio::select! {
        biased;
        _ = ctx.ct.cancelled() => {
            cancel_peer_request(handle, "parent tool request cancelled").await;
            return Err("Output-path elicitation was cancelled with the tool request.".to_string());
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(5 * 60)) => {
            cancel_peer_request(handle, "output-path elicitation timed out").await;
            return Err("Output-path elicitation timed out after 5 minutes. Please pass output_path.".to_string());
        }
        response = &mut handle.rx => response,
    }
    .map_err(|_| "The client connection closed during output-path elicitation.".to_string())?
    .map_err(|error| format!("Output-path elicitation failed: {error}"))?;

    let result = match response {
        ClientResult::ElicitResult(result) => result,
        _ => {
            return Err(
                "Client returned an unexpected response to output-path elicitation.".to_string(),
            );
        }
    };
    match result.action {
        ElicitationAction::Accept => {
            let content = result.content.ok_or_else(|| {
                "The client accepted output-path elicitation without providing content. Please pass output_path."
                    .to_string()
            })?;
            let response: OutputPathRequest = serde_json::from_value(content).map_err(|error| {
                format!("The elicited output_path could not be parsed: {error}")
            })?;
            if response.output_path.trim().is_empty() {
                Err("The elicited output_path was empty. Please provide a valid path.".to_string())
            } else {
                Ok(response.output_path)
            }
        }
        ElicitationAction::Decline => {
            Err("Output-path elicitation was declined. Please pass output_path.".to_string())
        }
        ElicitationAction::Cancel => {
            Err("Output-path elicitation was cancelled. Please pass output_path.".to_string())
        }
        _ => Err(
            "Client returned an unsupported output-path elicitation action. Please pass output_path."
                .to_string(),
        ),
    }
}

fn output_path_elicitation(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        "pix_gpu_capture" => Some((
            "process_id",
            "Where should the GPU capture (.wpix) be saved?",
        )),
        "pix_gpu_capture_launch" => {
            Some(("exe_path", "Where should the GPU capture (.wpix) be saved?"))
        }
        "pix_timing_capture" => Some((
            "process_id",
            "Where should the timing capture (.wpix) be saved?",
        )),
        "pix_get_screenshot" => Some(("capture_path", "Where should the screenshot PNG be saved?")),
        "pix_capture_and_analyze" => {
            Some(("exe_path", "Where should the capture (.wpix) be saved?"))
        }
        _ => None,
    }
}

/// Resolve missing destinations before acquiring an execution slot. This
/// prevents several unanswered forms from exhausting the global tool pool.
/// Calls with malformed/missing required arguments are left to the normal
/// schema validator and therefore never trigger an unnecessary form.
async fn preflight_output_path(
    context: &RequestContext<RoleServer>,
    request: &mut CallToolRequestParams,
) -> Result<(), String> {
    let Some((required_argument, prompt)) = output_path_elicitation(&request.name) else {
        return Ok(());
    };
    let Some(arguments) = request.arguments.as_mut() else {
        return Ok(());
    };
    let has_required_argument = arguments
        .get(required_argument)
        .is_some_and(|value| match value {
            serde_json::Value::String(value) => !value.trim().is_empty(),
            serde_json::Value::Number(_) => true,
            _ => false,
        });
    if !has_required_argument
        || arguments
            .get("output_path")
            .is_some_and(|value| !value.is_null())
    {
        return Ok(());
    }

    let output_path = resolve_output_path(context, None, prompt).await?;
    arguments.insert(
        "output_path".to_string(),
        serde_json::Value::String(output_path),
    );
    Ok(())
}

#[tool_router(router = tool_router)]
impl PixServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
            artifact_registry: resources::ArtifactRegistry::default(),
        }
    }

    #[tool(
        name = "pix_status",
        description = "Check PIX installation status: whether pixtool.exe is available and \
                       whether the server is running elevated (required for timing captures).",
        annotations(title = "PIX status", read_only_hint = true)
    )]
    async fn pix_status(
        &self,
        Parameters(_args): Parameters<EmptyArgs>,
    ) -> Result<Json<status::StatusReport>, String> {
        done(status::handle_pix_status().await)
    }

    #[tool(
        name = "pix_launch",
        description = "Launch an executable under pixtool. NOTE: this returns pixtool's launcher \
                       PID, not the game's, and does not leave a process you can later GPU-capture \
                       by PID. For a programmatic GPU capture use pix_gpu_capture_launch or \
                       pix_capture_and_analyze (PIX can only capture a process it launched).",
        annotations(title = "Launch with PIX", destructive_hint = true)
    )]
    async fn pix_launch(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(args): Parameters<launch::LaunchArgs>,
    ) -> Result<Json<launch::LaunchReport>, String> {
        done(
            with_progress(
                &context,
                "Launching application with PIX",
                launch::handle_pix_launch(args),
            )
            .await,
        )
    }

    #[tool(
        name = "pix_launch_and_capture",
        description = "Launch an executable with PIX and capture from start. Useful for \
                       capturing startup behavior or the first frames of an application.",
        annotations(title = "Launch and capture", destructive_hint = true),
        output_schema = rmcp::handler::server::tool::schema_for_output::<launch::LaunchReport>()
            .expect("LaunchReport must produce a valid object output schema"),
    )]
    async fn pix_launch_and_capture(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(args): Parameters<launch::LaunchAndCaptureArgs>,
    ) -> Result<CallToolResult, McpError> {
        let capture_path = args
            .capture_file
            .as_deref()
            .and_then(|path| capture::normalize_wpix_output(path, "capture_file").ok());
        let result = with_progress(
            &context,
            "Launching application and capturing",
            launch::handle_pix_launch_and_capture(args),
        )
        .await;
        structured_call_result(result, |_| {
            capture_path
                .as_deref()
                .and_then(|path| {
                    resources::local_artifact_resource(
                        &self.artifact_registry,
                        path,
                        "PIX startup capture",
                        "application/octet-stream",
                    )
                })
                .into_iter()
                .collect()
        })
    }

    #[tool(
        name = "pix_gpu_capture",
        description = "Take a GPU capture of a process PIX already launched (by PID), saving a \
                       .wpix file. IMPORTANT: PIX can only GPU-capture a process it launched \
                       itself; attaching to an independently-started game fails with PIXTOOL17. \
                       For a normal game, prefer pix_gpu_capture_launch or pix_capture_and_analyze.",
        annotations(title = "GPU capture (PID)", destructive_hint = true),
        output_schema = rmcp::handler::server::tool::schema_for_output::<capture::CaptureReport>()
            .expect("CaptureReport must produce a valid object output schema"),
    )]
    async fn pix_gpu_capture(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(mut args): Parameters<capture::GpuCaptureArgs>,
    ) -> Result<CallToolResult, McpError> {
        args.output_path = Some(
            match resolve_output_path(
                &context,
                args.output_path,
                "Where should the GPU capture (.wpix) be saved?",
            )
            .await
            {
                Ok(path) => path,
                Err(error) => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(error)]));
                }
            },
        );
        let result = with_progress(
            &context,
            "Taking GPU capture",
            capture::handle_pix_gpu_capture(args),
        )
        .await;
        structured_call_result(result, |report| {
            resources::local_artifact_resource(
                &self.artifact_registry,
                &report.output_path,
                "PIX GPU capture",
                "application/octet-stream",
            )
            .into_iter()
            .collect()
        })
    }

    #[tool(
        name = "pix_gpu_capture_launch",
        description = "Launch an executable and capture GPU frames to a .wpix file via pixtool.exe.",
        annotations(title = "Launch + GPU capture", destructive_hint = true),
        output_schema = rmcp::handler::server::tool::schema_for_output::<capture::CaptureReport>()
            .expect("CaptureReport must produce a valid object output schema"),
    )]
    async fn pix_gpu_capture_launch(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(mut args): Parameters<capture::GpuCaptureLaunchArgs>,
    ) -> Result<CallToolResult, McpError> {
        args.output_path = Some(
            match resolve_output_path(
                &context,
                args.output_path,
                "Where should the GPU capture (.wpix) be saved?",
            )
            .await
            {
                Ok(path) => path,
                Err(error) => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(error)]));
                }
            },
        );
        let result = with_progress(
            &context,
            "Launching application and taking GPU capture",
            capture::handle_pix_gpu_capture_launch(args),
        )
        .await;
        structured_call_result(result, |report| {
            resources::local_artifact_resource(
                &self.artifact_registry,
                &report.output_path,
                "PIX GPU capture",
                "application/octet-stream",
            )
            .into_iter()
            .collect()
        })
    }

    #[tool(
        name = "pix_timing_capture",
        description = "Take a timing capture of a running process (CPU/GPU timing). Requires \
                       administrator privileges.",
        annotations(title = "Timing capture (PID)", destructive_hint = true),
        output_schema = rmcp::handler::server::tool::schema_for_output::<capture::CaptureReport>()
            .expect("CaptureReport must produce a valid object output schema"),
    )]
    async fn pix_timing_capture(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(mut args): Parameters<capture::TimingCaptureArgs>,
    ) -> Result<CallToolResult, McpError> {
        args.output_path = Some(
            match resolve_output_path(
                &context,
                args.output_path,
                "Where should the timing capture (.wpix) be saved?",
            )
            .await
            {
                Ok(path) => path,
                Err(error) => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(error)]));
                }
            },
        );
        let result = with_progress(
            &context,
            "Taking PIX timing capture",
            capture::handle_pix_timing_capture(args),
        )
        .await;
        structured_call_result(result, |report| {
            resources::local_artifact_resource(
                &self.artifact_registry,
                &report.output_path,
                "PIX timing capture",
                "application/octet-stream",
            )
            .into_iter()
            .collect()
        })
    }

    #[tool(
        name = "pix_list_captures",
        description = "List PIX capture files (.wpix) in a directory with offset/limit pagination.",
        annotations(title = "List captures", read_only_hint = true)
    )]
    async fn pix_list_captures(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(args): Parameters<capture::ListCapturesArgs>,
    ) -> Result<Json<capture::CaptureListReport>, String> {
        done(
            with_progress(
                &context,
                "Scanning PIX captures",
                capture::handle_pix_list_captures(args),
            )
            .await,
        )
    }

    #[tool(
        name = "pix_open_capture",
        description = "Open a capture file in the PIX GUI for detailed analysis.",
        annotations(title = "Open capture in PIX")
    )]
    async fn pix_open_capture(
        &self,
        Parameters(args): Parameters<capture::OpenCaptureArgs>,
    ) -> Result<Json<capture::MessageReport>, String> {
        done(capture::handle_pix_open_capture(args).await)
    }

    #[tool(
        name = "pix_analyze_capture",
        description = "Analyze a .wpix capture: extract the event list (and optionally counters) \
                       and return structured analysis results.",
        annotations(title = "Analyze capture", read_only_hint = true)
    )]
    async fn pix_analyze_capture(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(args): Parameters<analysis::AnalyzeCaptureArgs>,
    ) -> Result<Json<analysis::AnalyzeReport>, String> {
        done(
            with_progress(
                &context,
                "Analyzing PIX capture",
                analysis::handle_pix_analyze_capture(args),
            )
            .await,
        )
    }

    #[tool(
        name = "pix_analyze_frame",
        description = "Heuristic frame triage from a capture: draw/dispatch/barrier counts, \
                       render-target changes, and the most expensive GPU events. Great first step \
                       for performance debugging.",
        annotations(title = "Analyze frame", read_only_hint = true)
    )]
    async fn pix_analyze_frame(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(args): Parameters<analysis::AnalyzeFrameArgs>,
    ) -> Result<Json<analysis::FrameInsights>, String> {
        done(
            with_progress(
                &context,
                "Analyzing captured frame",
                analysis::handle_pix_analyze_frame(args),
            )
            .await,
        )
    }

    #[tool(
        name = "pix_get_event_list",
        description = "Extract the event list (D3D12 API calls, draw calls, GPU events) from a \
                       capture. Returns a paginated slice (offset/limit/response_format) or saves \
                       the full CSV when output_path is given.",
        annotations(title = "Get event list", destructive_hint = true),
        output_schema = rmcp::handler::server::tool::schema_for_output::<analysis::EventListReport>()
            .expect("EventListReport must produce a valid object output schema"),
    )]
    async fn pix_get_event_list(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(args): Parameters<analysis::EventListArgs>,
    ) -> Result<CallToolResult, McpError> {
        let result = with_progress(
            &context,
            "Extracting PIX event list",
            analysis::handle_pix_get_event_list(args),
        )
        .await;
        structured_call_result(result, |report| {
            report
                .output_path
                .as_deref()
                .and_then(|path| {
                    resources::local_artifact_resource(
                        &self.artifact_registry,
                        path,
                        "PIX event list",
                        "text/csv",
                    )
                })
                .into_iter()
                .collect()
        })
    }

    #[tool(
        name = "pix_get_screenshot",
        description = "Extract the frame recorded with the capture as a PNG (pixtool \
                       save-screenshot) and return it inline as an image so a vision model can \
                       inspect the render. Set depth=true, marker=<name>, global_id=<id>, or \
                       rtv_index=<n> to instead save a specific render target / depth buffer via \
                       capture replay (save-resource).",
        annotations(title = "Get screenshot", destructive_hint = true),
        output_schema = rmcp::handler::server::tool::schema_for_output::<analysis::ScreenshotReport>()
            .expect("ScreenshotReport must produce a valid object output schema"),
    )]
    async fn pix_get_screenshot(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(args): Parameters<analysis::ScreenshotArgs>,
    ) -> Result<CallToolResult, McpError> {
        let output_path = match resolve_output_path(
            &context,
            args.output_path.clone(),
            "Where should the screenshot PNG be saved?",
        )
        .await
        {
            Ok(p) => p,
            Err(e) => return Ok(CallToolResult::error(vec![ContentBlock::text(e)])),
        };

        let embed = args.embed_image.unwrap_or(true);
        let max_dim = args.max_dimension.unwrap_or(1280);
        let depth = args.depth.unwrap_or(false);
        let marker = args.marker.clone();
        let global_id = args.global_id;
        let rtv_index = args.rtv_index;
        let capture_path = args.capture_path;

        let result = with_progress(
            &context,
            "Extracting PIX screenshot",
            analysis::handle_pix_get_screenshot(analysis::ScreenshotRequest {
                capture_path,
                output_path,
                depth,
                marker,
                global_id,
                rtv_index,
                embed_image: embed,
                max_dimension: max_dim,
                replace_existing: true,
            }),
        )
        .await;
        match result {
            Ok(res) => screenshot_call_result(&self.artifact_registry, res),
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(
                e.to_string(),
            )])),
        }
    }

    #[tool(
        name = "pix_list_counters",
        description = "List the available performance counters for a capture (supports a \
                       case-insensitive filter and a limit).",
        annotations(title = "List counters", read_only_hint = true)
    )]
    async fn pix_list_counters(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(args): Parameters<analysis::ListCountersArgs>,
    ) -> Result<Json<analysis::CountersReport>, String> {
        done(
            with_progress(
                &context,
                "Listing PIX performance counters",
                analysis::handle_pix_list_counters(args),
            )
            .await,
        )
    }

    #[tool(
        name = "pix_run_analysis",
        description = "Replay a capture with the D3D12 debug layer enabled and report playback \
                       diagnostics. pixtool does not export the debug layer's messages, so this \
                       validates replay rather than claiming a complete error/warning inventory.",
        annotations(title = "Run debug-layer analysis", read_only_hint = true)
    )]
    async fn pix_run_analysis(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(args): Parameters<analysis::CapturePathArgs>,
    ) -> Result<Json<analysis::RunAnalysisReport>, String> {
        done(
            with_progress(
                &context,
                "Replaying capture with the debug layer",
                analysis::handle_pix_run_analysis(args),
            )
            .await,
        )
    }

    #[tool(
        name = "pix_export_counters",
        description = "Parse a PIX-exported counters file (CSV or JSON) into structured data.",
        annotations(title = "Export counters", read_only_hint = true)
    )]
    async fn pix_export_counters(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(args): Parameters<analysis::ExportCountersArgs>,
    ) -> Result<Json<analysis::ExportCountersReport>, String> {
        done(
            with_progress(
                &context,
                "Parsing PIX counter export",
                analysis::handle_pix_export_counters(args),
            )
            .await,
        )
    }

    #[tool(
        name = "pix_compare_captures",
        description = "Compare two capture files' size and modification metadata. This does not \
                       establish a performance regression; compare event timing for that.",
        annotations(title = "Compare captures", read_only_hint = true)
    )]
    async fn pix_compare_captures(
        &self,
        Parameters(args): Parameters<analysis::CompareCapturesArgs>,
    ) -> Result<Json<analysis::CompareReport>, String> {
        done(analysis::handle_pix_compare_captures(args).await)
    }

    #[tool(
        name = "pix_capture_and_analyze",
        description = "One-shot workflow: launch an executable, take a GPU capture, and return a \
                       frame-insights summary (and optionally a screenshot). Fewer round-trips \
                       than launch + capture + analyze separately.",
        annotations(title = "Capture and analyze", destructive_hint = true),
        output_schema = rmcp::handler::server::tool::schema_for_output::<workflow::CaptureAndAnalyzeReport>()
            .expect("CaptureAndAnalyzeReport must produce a valid object output schema"),
    )]
    async fn pix_capture_and_analyze(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(mut args): Parameters<workflow::CaptureAndAnalyzeArgs>,
    ) -> Result<CallToolResult, McpError> {
        args.output_path = Some(
            match resolve_output_path(
                &context,
                args.output_path,
                "Where should the capture (.wpix) be saved?",
            )
            .await
            {
                Ok(path) => path,
                Err(error) => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(error)]));
                }
            },
        );
        let result = with_progress(
            &context,
            "Capturing and analyzing PIX frame",
            workflow::handle_pix_capture_and_analyze(args),
        )
        .await;
        structured_call_result(result, |report| {
            let mut links = Vec::new();
            if let Some(resource) = resources::local_artifact_resource(
                &self.artifact_registry,
                &report.capture_path,
                "PIX GPU capture",
                "application/octet-stream",
            ) {
                links.push(resource);
            }
            if let Some(resource) = report.screenshot_path.as_deref().and_then(|path| {
                resources::local_artifact_resource(
                    &self.artifact_registry,
                    path,
                    "PIX screenshot",
                    "image/png",
                )
            }) {
                links.push(resource);
            }
            links
        })
    }
}

impl ServerHandler for PixServer {
    fn get_info(&self) -> ServerInfo {
        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .build();

        ServerInfo::new(capabilities)
            .with_server_info(
                Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
                    .with_title("PIX MCP Server")
                    .with_description("MCP server for Microsoft PIX DirectX 12 GPU debugging"),
            )
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_instructions(
                "Microsoft PIX debugging tools for DirectX 12. Run pix_status first to verify \
                 setup. Use pix_capture_and_analyze for a one-shot launch+capture+triage, or \
                 pix_gpu_capture_launch / pix_gpu_capture to record. Analyze with \
                 pix_analyze_frame (heuristic triage), pix_get_event_list (paginated), \
                 pix_get_screenshot (returns the frame as an image), pix_list_counters, and \
                 pix_run_analysis (playback validation only; pixtool does not export debug-layer \
                 messages). Tool cancellation terminates managed pixtool subprocesses. \
                 pix_open_capture opens the PIX GUI."
                    .to_string(),
            )
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let cancellation = context.ct.clone();
        let call = self
            .tool_router
            .call(ToolCallContext::new(self, request, context));

        // rmcp exposes request cancellation through RequestContext. Selecting
        // on it here ensures the actual tool future is dropped, which in turn
        // activates the pixtool process-tree and temporary-artifact guards.
        tokio::select! {
            biased;
            result = call => result,
            _ = cancellation.cancelled() => {
                Err(McpError::internal_error("Tool request cancelled", None))
            }
        }
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        reject_cursor(request.as_ref(), "tools/list")?;
        let mut tools = self.tool_router.list_all();
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(ListToolsResult::with_all_items(tools))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tool_router.get(name).cloned()
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let captures_dir = crate::security::capture_directory().map_err(|error| {
            McpError::internal_error(
                "Could not resolve the PIX capture resource directory",
                Some(serde_json::json!({ "reason": error.to_string() })),
            )
        })?;
        let resources = resources::list_capture_resources(Some(&captures_dir))
            .await
            .map_err(|error| {
                McpError::internal_error(
                    "Could not build the PIX capture resource catalog",
                    Some(serde_json::json!({ "reason": error.to_string() })),
                )
            })?;
        let total = resources.len();
        let generation = resource_catalog_generation(&resources);
        let offset = resource_cursor_offset(request.as_ref(), generation, total)?;
        let page: Vec<_> = resources
            .into_iter()
            .skip(offset)
            .take(RESOURCE_PAGE_SIZE)
            .collect();
        let next_offset = offset.saturating_add(page.len());
        let mut result = ListResourcesResult::with_all_items(page);
        if next_offset < total {
            result.next_cursor = Some(format!(
                "{RESOURCE_CURSOR_PREFIX}{generation:016x}:{next_offset}"
            ));
        }
        Ok(result)
    }

    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        reject_cursor(request.as_ref(), "resources/templates/list")?;
        let annotations = Annotations::default()
            .with_audience(vec![Role::Assistant])
            .with_priority(0.8);
        let mut templates = vec![
            ResourceTemplate::new("capture://{id}", "PIX capture")
                .with_description("Metadata for a .wpix capture in the server capture directory")
                .with_mime_type("application/json")
                .with_annotations(annotations.clone()),
            ResourceTemplate::new("capture://{id}/metadata", "PIX capture metadata")
                .with_description("File metadata for a .wpix capture")
                .with_mime_type("application/json")
                .with_annotations(annotations.clone()),
            ResourceTemplate::new("capture://{id}/events", "PIX capture events")
                .with_description("A validated capture reference and guidance for event export")
                .with_mime_type("application/json")
                .with_annotations(annotations.clone()),
            ResourceTemplate::new("capture://{id}/counters", "PIX capture counters")
                .with_description("A validated capture reference and guidance for counters")
                .with_mime_type("application/json")
                .with_annotations(annotations),
        ];
        templates.sort_by(|left, right| left.uri_template.cmp(&right.uri_template));
        Ok(ListResourceTemplatesResult::with_all_items(templates))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let uri = request.uri.clone();
        if uri.starts_with("artifact://") {
            let payload = resources::read_artifact_resource(&self.artifact_registry, &uri)
                .await
                .map_err(|error| map_resource_error(error, &uri))?;
            let contents = match payload {
                resources::ArtifactPayload::Text { text, mime_type } => {
                    ResourceContents::text(text, uri).with_mime_type(mime_type)
                }
                resources::ArtifactPayload::Blob { base64, mime_type } => {
                    ResourceContents::blob(base64, uri).with_mime_type(mime_type)
                }
            };
            return Ok(ReadResourceResult::new(vec![contents]));
        }
        match resources::read_capture_resource_text(&uri, None).await {
            Ok(text) => Ok(ReadResourceResult::new(vec![
                ResourceContents::text(text, uri).with_mime_type("application/json"),
            ])),
            Err(error) => Err(map_resource_error(error, &uri)),
        }
    }
}

impl rmcp::Service<RoleServer> for PixService {
    async fn handle_request(
        &self,
        request: ClientRequest,
        context: RequestContext<RoleServer>,
    ) -> Result<ServerResult, McpError> {
        match request {
            ClientRequest::CallToolRequest(mut request) => {
                // Tasks are not advertised. MCP 2025-11-25 requires the
                // receiver to ignore task metadata and process the call
                // normally in that case.
                request.params.task = None;

                if let Err(error) = preflight_output_path(&context, &mut request.params).await {
                    return Ok(ServerResult::CallToolResult(CallToolResult::error(vec![
                        ContentBlock::text(error),
                    ])));
                }

                let _permit =
                    match acquire_tool_permit(self.tool_concurrency.clone(), &context).await {
                        Ok(permit) => permit,
                        Err(ToolPermitError::Busy) => {
                            return Ok(ServerResult::CallToolResult(CallToolResult::error(vec![
                                ContentBlock::text(
                                    "Server is busy: timed out waiting for a tool execution slot",
                                ),
                            ])));
                        }
                        Err(ToolPermitError::Cancelled) => {
                            return Err(McpError::internal_error("Tool request cancelled", None));
                        }
                        Err(ToolPermitError::Closed) => {
                            return Err(McpError::internal_error(
                                "Tool concurrency limiter is unavailable",
                                None,
                            ));
                        }
                    };
                self.server
                    .call_tool(request.params, context)
                    .await
                    .map(ServerResult::CallToolResult)
            }
            request => {
                <PixServer as rmcp::Service<RoleServer>>::handle_request(
                    &self.server,
                    request,
                    context,
                )
                .await
            }
        }
    }

    async fn handle_notification(
        &self,
        notification: ClientNotification,
        context: rmcp::service::NotificationContext<RoleServer>,
    ) -> Result<(), McpError> {
        <PixServer as rmcp::Service<RoleServer>>::handle_notification(
            &self.server,
            notification,
            context,
        )
        .await
    }

    fn get_info(&self) -> ServerInfo {
        <PixServer as rmcp::Service<RoleServer>>::get_info(&self.server)
    }
}

async fn acquire_tool_permit(
    semaphore: Arc<Semaphore>,
    context: &RequestContext<RoleServer>,
) -> Result<OwnedSemaphorePermit, ToolPermitError> {
    let acquire = semaphore.acquire_owned();
    tokio::pin!(acquire);
    let timeout = tokio::time::sleep(TOOL_QUEUE_TIMEOUT);
    tokio::pin!(timeout);

    tokio::select! {
        biased;
        _ = context.ct.cancelled() => Err(ToolPermitError::Cancelled),
        permit = &mut acquire => permit.map_err(|_| ToolPermitError::Closed),
        _ = &mut timeout => Err(ToolPermitError::Busy),
    }
}

/// Build a new PIX MCP server instance.
pub fn create_server() -> AnyResult<PixService> {
    Ok(PixService {
        server: PixServer::new(),
        tool_concurrency: Arc::new(Semaphore::new(crate::security::max_concurrent_tools()?)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_advertises_structured_output() {
        let server = PixServer::new();
        let tools = server.tool_router.list_all();

        assert!(!tools.is_empty());
        for tool in tools {
            assert!(
                tool.output_schema.is_some(),
                "tool {} is missing outputSchema",
                tool.name
            );
        }
    }

    #[test]
    fn every_tool_rejects_unknown_input_fields_in_its_schema() {
        let server = PixServer::new();
        for tool in server.tool_router.list_all() {
            assert_eq!(
                tool.input_schema.get("additionalProperties"),
                Some(&serde_json::Value::Bool(false)),
                "tool {} must advertise additionalProperties=false",
                tool.name
            );
        }
    }

    #[test]
    fn file_writing_tools_are_not_marked_read_only() {
        let server = PixServer::new();
        let tools = server.tool_router.list_all();

        for name in [
            "pix_launch_and_capture",
            "pix_gpu_capture",
            "pix_gpu_capture_launch",
            "pix_timing_capture",
            "pix_get_event_list",
            "pix_get_screenshot",
            "pix_capture_and_analyze",
        ] {
            let tool = tools
                .iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("missing tool {name}"));
            let annotations = tool.annotations.as_ref().expect("tool annotations");
            assert_ne!(annotations.read_only_hint, Some(true), "{name}");
            assert_eq!(annotations.destructive_hint, Some(true), "{name}");
        }
    }

    #[test]
    fn screenshot_response_mirrors_structured_content_as_json_text() {
        let directory = tempfile::tempdir().expect("artifact directory");
        let screenshot = directory.path().join("frame.png");
        std::fs::write(&screenshot, b"png").expect("write screenshot fixture");
        let registry = resources::ArtifactRegistry::default();
        let result = screenshot_call_result(
            &registry,
            analysis::ScreenshotResult {
                report: analysis::ScreenshotReport {
                    success: true,
                    output_path: screenshot.to_string_lossy().into_owned(),
                    file_size_bytes: 3,
                    message: "Screenshot saved".to_string(),
                    image_embedded: true,
                },
                image_b64: Some("cG5n".to_string()),
            },
        )
        .expect("screenshot response should serialize");

        let structured = result
            .structured_content
            .as_ref()
            .expect("structured content");
        assert_eq!(result.content.len(), 3);
        assert_eq!(result.content[0].as_image().expect("image").data, "cG5n");

        let text = &result.content[1].as_text().expect("JSON text").text;
        assert_eq!(text, &structured.to_string());
        let text_json: serde_json::Value =
            serde_json::from_str(text).expect("text must contain valid JSON");
        assert_eq!(&text_json, structured);

        let resource = result.content[2]
            .as_resource_link()
            .expect("screenshot resource link");
        assert_eq!(resource.mime_type.as_deref(), Some("image/png"));
        assert!(resource.uri.starts_with("artifact://local/"));
    }

    #[test]
    fn server_does_not_advertise_broken_upstream_task_execution() {
        let info = PixServer::new().get_info();
        assert!(info.capabilities.tasks.is_none());
    }

    #[test]
    fn server_does_not_overclaim_resource_catalog_changes() {
        let info = PixServer::new().get_info();
        assert_eq!(
            info.capabilities
                .resources
                .and_then(|resources| resources.list_changed),
            None
        );
    }

    #[test]
    fn progress_gate_is_monotone_and_rate_limited() {
        let start = tokio::time::Instant::now();
        let mut gate = ProgressGate::default();

        assert!(gate.admit(0.0, start, true));
        assert!(!gate.admit(0.0, start, true));
        assert!(!gate.admit(1.0, start + PROGRESS_NOTIFICATION_INTERVAL / 2, false));
        assert!(gate.admit(2.0, start + PROGRESS_NOTIFICATION_INTERVAL, false));
        assert!(!gate.admit(1.5, start + PROGRESS_NOTIFICATION_INTERVAL * 2, true));
        assert!(gate.admit(3.0, start + PROGRESS_NOTIFICATION_INTERVAL, true));
    }

    #[test]
    fn resource_cursors_are_opaque_and_strictly_validated() {
        let generation = 0x1234;
        let total = 250;
        let first_page = PaginatedRequestParams::default();
        assert_eq!(
            resource_cursor_offset(Some(&first_page), generation, total).unwrap(),
            0
        );

        let second_page = PaginatedRequestParams::default().with_cursor(Some(format!(
            "{RESOURCE_CURSOR_PREFIX}{generation:016x}:100"
        )));
        assert_eq!(
            resource_cursor_offset(Some(&second_page), generation, total).unwrap(),
            100
        );

        let unknown = PaginatedRequestParams::default().with_cursor(Some("100".to_string()));
        assert!(resource_cursor_offset(Some(&unknown), generation, total).is_err());
        assert!(reject_cursor(Some(&unknown), "tools/list").is_err());

        let stale = PaginatedRequestParams::default().with_cursor(Some(format!(
            "{RESOURCE_CURSOR_PREFIX}0000000000009999:100"
        )));
        assert!(resource_cursor_offset(Some(&stale), generation, total).is_err());

        let fabricated = PaginatedRequestParams::default()
            .with_cursor(Some(format!("{RESOURCE_CURSOR_PREFIX}{generation:016x}:1")));
        assert!(resource_cursor_offset(Some(&fabricated), generation, total).is_err());
    }

    #[test]
    fn structured_results_can_include_artifact_links() {
        let directory = tempfile::tempdir().expect("artifact directory");
        let capture = directory.path().join("capture.wpix");
        std::fs::write(&capture, b"capture").expect("write capture");
        let path = capture.to_string_lossy().into_owned();
        let registry = resources::ArtifactRegistry::default();

        let result = structured_call_result(
            Ok(capture::CaptureReport {
                success: true,
                message: "captured".to_string(),
                output_path: path,
                pixtool_output: String::new(),
                note: None,
            }),
            |report| {
                resources::local_artifact_resource(
                    &registry,
                    &report.output_path,
                    "PIX GPU capture",
                    "application/octet-stream",
                )
                .into_iter()
                .collect()
            },
        )
        .expect("structured tool result");

        assert!(result.structured_content.is_some());
        let link = result
            .content
            .iter()
            .find_map(ContentBlock::as_resource_link)
            .expect("capture resource link");
        assert_eq!(link.size, None);
        assert_eq!(link.mime_type.as_deref(), Some("application/json"));
        assert!(link.uri.starts_with("artifact://local/"));
    }
}
