//! MCP server exposing Microsoft PIX debugging tools via the official `rmcp` SDK.

mod analysis;
mod capture;
mod launch;
mod resources;
mod status;
mod workflow;

use anyhow::Result as AnyResult;
use std::sync::Arc;

use rmcp::{
    elicit_safe,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars,
    service::RequestContext,
    task_handler,
    task_manager::OperationProcessor,
    tool, tool_handler, tool_router, ErrorData as McpError, Json, RoleServer, ServerHandler,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// The PIX MCP server. Tools are registered through the `#[tool_router]` macro.
#[derive(Clone)]
pub struct PixServer {
    tool_router: ToolRouter<Self>,
    /// Backs MCP task support (`#[task_handler]`): long-running tools may be
    /// invoked as durable tasks and polled by the client.
    processor: Arc<Mutex<OperationProcessor>>,
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
    /// Absolute path for the output file.
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

/// Resolve an output path, asking the client via elicitation when it is missing.
/// Returns an actionable tool error if no path can be obtained.
async fn resolve_output_path(
    ctx: &RequestContext<RoleServer>,
    provided: Option<String>,
    prompt: &str,
) -> Result<String, String> {
    if let Some(p) = provided {
        return Ok(p);
    }
    match ctx
        .peer
        .elicit::<OutputPathRequest>(prompt.to_string())
        .await
    {
        Ok(Some(r)) => Ok(r.output_path),
        Ok(None) => {
            Err("Output path not provided (elicitation declined). Please pass output_path."
                .to_string())
        }
        Err(_) => Err(
            "output_path is required; the client does not support elicitation. Please pass output_path."
                .to_string(),
        ),
    }
}

#[tool_router(router = tool_router)]
impl PixServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
            processor: Arc::new(Mutex::new(OperationProcessor::new())),
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
        annotations(title = "GPU capture (PID)"),
        execution(task_support = "optional")
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
        annotations(title = "Launch + GPU capture", destructive_hint = true),
        execution(task_support = "optional")
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
        annotations(title = "Timing capture (PID)"),
        execution(task_support = "optional")
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
        description = "List all PIX capture files (.wpix) in a directory.",
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
        annotations(title = "Analyze capture", read_only_hint = true),
        execution(task_support = "optional")
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
        annotations(title = "Analyze frame", read_only_hint = true),
        execution(task_support = "optional")
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
        annotations(title = "Get event list", read_only_hint = true),
        execution(task_support = "optional")
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
        annotations(title = "Get screenshot", read_only_hint = true),
        execution(task_support = "optional")
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
            Err(e) => return Ok(CallToolResult::error(vec![Content::text(e)])),
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
        })
        .await
        {
            Ok(res) => {
                let mut content = Vec::new();
                if let Some(b64) = res.image_b64 {
                    content.push(Content::image(b64, "image/png"));
                }
                content.push(Content::text(res.report.message.clone()));
                let mut result = CallToolResult::success(content);
                result.structured_content = serde_json::to_value(&res.report).ok();
                Ok(result)
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
        }
    }

    #[tool(
        name = "pix_list_counters",
        description = "List the available performance counters for a capture (supports a \
                       case-insensitive filter and a limit).",
        annotations(title = "List counters", read_only_hint = true),
        execution(task_support = "optional")
    )]
    async fn pix_list_counters(
        &self,
        Parameters(args): Parameters<analysis::ListCountersArgs>,
    ) -> Result<Json<analysis::CountersReport>, String> {
        done(analysis::handle_pix_list_counters(args).await)
    }

    #[tool(
        name = "pix_run_analysis",
        description = "Replay a capture with the D3D12 debug layer enabled to detect errors, \
                       warnings, and validation issues.",
        annotations(title = "Run debug-layer analysis", read_only_hint = true),
        execution(task_support = "optional")
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
        description = "Compare two capture files (size/modification time) for regression detection.",
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
        execution(task_support = "optional")
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

#[tool_handler(router = self.tool_router)]
#[task_handler]
impl ServerHandler for PixServer {
    fn get_info(&self) -> ServerInfo {
        let mut capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .build();
        // Advertise MCP task support so clients may invoke long-running tools as
        // durable, pollable tasks (tasks.requests.tools.call).
        capabilities.tasks = Some(TasksCapability::server_default());

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
                 pix_run_analysis. Long-running tools support MCP tasks. pix_open_capture opens \
                 the PIX GUI."
                    .to_string(),
            )
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult {
            resources: vec![RawResource::new("capture://list", "PIX capture list").no_annotation()],
            next_cursor: None,
            meta: None,
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let uri = request.uri.clone();
        match resources::read_capture_resource_text(&uri, None).await {
            Ok(text) => Ok(ReadResourceResult::new(vec![ResourceContents::text(
                text, uri,
            )])),
            Err(e) => Err(McpError::resource_not_found(
                e.to_string(),
                Some(serde_json::json!({ "uri": uri })),
            )),
        }
    }
}

/// Build a new PIX MCP server instance.
pub fn create_server() -> PixServer {
    PixServer::new()
}
