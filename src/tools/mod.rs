//! MCP server exposing Microsoft PIX debugging tools via the official `rmcp` SDK.

mod analysis;
mod capture;
mod launch;
mod resources;
mod status;
mod workflow;

use anyhow::Result as AnyResult;
use rmcp::{
    ErrorData as McpError, Json, RoleServer, ServerHandler, elicit_safe,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::*,
    schemars,
    service::{ElicitationMode, PeerRequestOptions, RequestContext, RequestHandle},
    tool, tool_router,
};
use serde::{Deserialize, Serialize};

/// The PIX MCP server. Tools are registered through the `#[tool_router]` macro.
#[derive(Clone)]
pub struct PixServer {
    tool_router: ToolRouter<Self>,
}

/// Service wrapper that normalizes task-augmented tool calls to direct calls.
/// rmcp 2.2 otherwise rejects/enqueues them even when the server correctly
/// omits the Tasks capability, contrary to the MCP fallback requirement.
#[derive(Clone)]
pub struct PixService {
    server: PixServer,
}

impl Default for PixServer {
    fn default() -> Self {
        Self::new()
    }
}

/// Elicitation payload used to ask the client for a missing output path.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(description = "Destination file path")]
struct OutputPathRequest {
    /// Path for the output file (absolute paths are recommended).
    #[schemars(length(min = 1))]
    output_path: String,
}
elicit_safe!(OutputPathRequest);

/// Wrap a logic-handler result for an MCP tool. Success becomes a typed
/// `Json<T>` value (producing an `outputSchema` and `structuredContent` plus a
/// text mirror); failures become `isError` tool content via `Err(String)` so
/// the model can read and self-correct (SEP-1303).
fn done<T>(result: AnyResult<T>) -> Result<Json<T>, String> {
    result.map(Json).map_err(|e| e.to_string())
}

/// Build the mixed image/structured response used by `pix_get_screenshot`.
/// The JSON text mirrors `structuredContent` for clients that only consume
/// content blocks.
fn screenshot_call_result(res: analysis::ScreenshotResult) -> Result<CallToolResult, McpError> {
    let structured_content = serde_json::to_value(&res.report).map_err(|e| {
        McpError::internal_error(format!("Failed to serialize screenshot report: {e}"), None)
    })?;

    let mut result = CallToolResult::structured(structured_content);
    if let Some(b64) = res.image_b64 {
        result
            .content
            .insert(0, ContentBlock::image(b64, "image/png"));
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

#[tool_router(router = tool_router)]
impl PixServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "pix_status",
        description = "Check PIX installation status: whether pixtool.exe is available and \
                       whether the server is running elevated (required for timing captures).",
        annotations(title = "PIX status", read_only_hint = true)
    )]
    async fn pix_status(&self) -> Result<Json<status::StatusReport>, String> {
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
        Parameters(args): Parameters<launch::LaunchArgs>,
    ) -> Result<Json<launch::LaunchReport>, String> {
        done(launch::handle_pix_launch(args).await)
    }

    #[tool(
        name = "pix_launch_and_capture",
        description = "Launch an executable with PIX and capture from start. Useful for \
                       capturing startup behavior or the first frames of an application.",
        annotations(title = "Launch and capture", destructive_hint = true)
    )]
    async fn pix_launch_and_capture(
        &self,
        Parameters(args): Parameters<launch::LaunchAndCaptureArgs>,
    ) -> Result<Json<launch::LaunchReport>, String> {
        done(launch::handle_pix_launch_and_capture(args).await)
    }

    #[tool(
        name = "pix_gpu_capture",
        description = "Take a GPU capture of a process PIX already launched (by PID), saving a \
                       .wpix file. IMPORTANT: PIX can only GPU-capture a process it launched \
                       itself; attaching to an independently-started game fails with PIXTOOL17. \
                       For a normal game, prefer pix_gpu_capture_launch or pix_capture_and_analyze.",
        annotations(title = "GPU capture (PID)", destructive_hint = true)
    )]
    async fn pix_gpu_capture(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(mut args): Parameters<capture::GpuCaptureArgs>,
    ) -> Result<Json<capture::CaptureReport>, String> {
        args.output_path = Some(
            resolve_output_path(
                &context,
                args.output_path,
                "Where should the GPU capture (.wpix) be saved?",
            )
            .await?,
        );
        done(capture::handle_pix_gpu_capture(args).await)
    }

    #[tool(
        name = "pix_gpu_capture_launch",
        description = "Launch an executable and capture GPU frames to a .wpix file via pixtool.exe.",
        annotations(title = "Launch + GPU capture", destructive_hint = true)
    )]
    async fn pix_gpu_capture_launch(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(mut args): Parameters<capture::GpuCaptureLaunchArgs>,
    ) -> Result<Json<capture::CaptureReport>, String> {
        args.output_path = Some(
            resolve_output_path(
                &context,
                args.output_path,
                "Where should the GPU capture (.wpix) be saved?",
            )
            .await?,
        );
        done(capture::handle_pix_gpu_capture_launch(args).await)
    }

    #[tool(
        name = "pix_timing_capture",
        description = "Take a timing capture of a running process (CPU/GPU timing). Requires \
                       administrator privileges.",
        annotations(title = "Timing capture (PID)", destructive_hint = true)
    )]
    async fn pix_timing_capture(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(mut args): Parameters<capture::TimingCaptureArgs>,
    ) -> Result<Json<capture::CaptureReport>, String> {
        args.output_path = Some(
            resolve_output_path(
                &context,
                args.output_path,
                "Where should the timing capture (.wpix) be saved?",
            )
            .await?,
        );
        done(capture::handle_pix_timing_capture(args).await)
    }

    #[tool(
        name = "pix_list_captures",
        description = "List PIX capture files (.wpix) in a directory with offset/limit pagination.",
        annotations(title = "List captures", read_only_hint = true)
    )]
    async fn pix_list_captures(
        &self,
        Parameters(args): Parameters<capture::ListCapturesArgs>,
    ) -> Result<Json<capture::CaptureListReport>, String> {
        done(capture::handle_pix_list_captures(args).await)
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
        Parameters(args): Parameters<analysis::AnalyzeCaptureArgs>,
    ) -> Result<Json<analysis::AnalyzeReport>, String> {
        done(analysis::handle_pix_analyze_capture(args).await)
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
        Parameters(args): Parameters<analysis::AnalyzeFrameArgs>,
    ) -> Result<Json<analysis::FrameInsights>, String> {
        done(analysis::handle_pix_analyze_frame(args).await)
    }

    #[tool(
        name = "pix_get_event_list",
        description = "Extract the event list (D3D12 API calls, draw calls, GPU events) from a \
                       capture. Returns a paginated slice (offset/limit/response_format) or saves \
                       the full CSV when output_path is given.",
        annotations(title = "Get event list", destructive_hint = true)
    )]
    async fn pix_get_event_list(
        &self,
        Parameters(args): Parameters<analysis::EventListArgs>,
    ) -> Result<Json<analysis::EventListReport>, String> {
        done(analysis::handle_pix_get_event_list(args).await)
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

        match analysis::handle_pix_get_screenshot(analysis::ScreenshotRequest {
            capture_path,
            output_path,
            depth,
            marker,
            global_id,
            rtv_index,
            embed_image: embed,
            max_dimension: max_dim,
            replace_existing: true,
        })
        .await
        {
            Ok(res) => screenshot_call_result(res),
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
        Parameters(args): Parameters<analysis::ListCountersArgs>,
    ) -> Result<Json<analysis::CountersReport>, String> {
        done(analysis::handle_pix_list_counters(args).await)
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
        Parameters(args): Parameters<analysis::CapturePathArgs>,
    ) -> Result<Json<analysis::RunAnalysisReport>, String> {
        done(analysis::handle_pix_run_analysis(args).await)
    }

    #[tool(
        name = "pix_export_counters",
        description = "Parse a PIX-exported counters file (CSV or JSON) into structured data.",
        annotations(title = "Export counters", read_only_hint = true)
    )]
    async fn pix_export_counters(
        &self,
        Parameters(args): Parameters<analysis::ExportCountersArgs>,
    ) -> Result<Json<analysis::ExportCountersReport>, String> {
        done(analysis::handle_pix_export_counters(args).await)
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
        annotations(title = "Capture and analyze", destructive_hint = true)
    )]
    async fn pix_capture_and_analyze(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(mut args): Parameters<workflow::CaptureAndAnalyzeArgs>,
    ) -> Result<Json<workflow::CaptureAndAnalyzeReport>, String> {
        args.output_path = Some(
            resolve_output_path(
                &context,
                args.output_path,
                "Where should the capture (.wpix) be saved?",
            )
            .await?,
        );
        done(workflow::handle_pix_capture_and_analyze(args).await)
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
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.tool_router.list_all()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tool_router.get(name).cloned()
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult::with_all_items(vec![
            Resource::new("capture://list", "PIX capture list").with_mime_type("application/json"),
        ]))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        Ok(ListResourceTemplatesResult::with_all_items(vec![
            ResourceTemplate::new("capture://{id}", "PIX capture")
                .with_description("Metadata for a .wpix capture in the server capture directory")
                .with_mime_type("application/json"),
            ResourceTemplate::new("capture://{id}/metadata", "PIX capture metadata")
                .with_description("File metadata for a .wpix capture")
                .with_mime_type("application/json"),
            ResourceTemplate::new("capture://{id}/events", "PIX capture events")
                .with_description("A validated capture reference and guidance for event export")
                .with_mime_type("application/json"),
            ResourceTemplate::new("capture://{id}/counters", "PIX capture counters")
                .with_description("A validated capture reference and guidance for counters")
                .with_mime_type("application/json"),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let uri = request.uri.clone();
        match resources::read_capture_resource_text(&uri, None).await {
            Ok(text) => Ok(ReadResourceResult::new(vec![
                ResourceContents::text(text, uri).with_mime_type("application/json"),
            ])),
            Err(e) => Err(McpError::resource_not_found(
                e.to_string(),
                Some(serde_json::json!({ "uri": uri })),
            )),
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

/// Build a new PIX MCP server instance.
pub fn create_server() -> PixService {
    PixService {
        server: PixServer::new(),
    }
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
    fn file_writing_tools_are_not_marked_read_only() {
        let server = PixServer::new();
        let tools = server.tool_router.list_all();

        for name in [
            "pix_gpu_capture",
            "pix_gpu_capture_launch",
            "pix_timing_capture",
            "pix_get_event_list",
            "pix_get_screenshot",
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
        let result = screenshot_call_result(analysis::ScreenshotResult {
            report: analysis::ScreenshotReport {
                success: true,
                output_path: r"C:\captures\frame.png".to_string(),
                file_size_bytes: 42,
                message: "Screenshot saved".to_string(),
                image_embedded: true,
            },
            image_b64: Some("cG5n".to_string()),
        })
        .expect("screenshot response should serialize");

        let structured = result
            .structured_content
            .as_ref()
            .expect("structured content");
        assert_eq!(result.content.len(), 2);
        assert_eq!(result.content[0].as_image().expect("image").data, "cG5n");

        let text = &result.content[1].as_text().expect("JSON text").text;
        assert_eq!(text, &structured.to_string());
        let text_json: serde_json::Value =
            serde_json::from_str(text).expect("text must contain valid JSON");
        assert_eq!(&text_json, structured);
    }

    #[test]
    fn server_does_not_advertise_broken_upstream_task_execution() {
        let info = PixServer::new().get_info();
        assert!(info.capabilities.tasks.is_none());
    }
}
