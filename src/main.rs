//! PIX MCP Server - Model Context Protocol server for Microsoft PIX debugging
//!
//! This server exposes PIX GPU capture and debugging tools to AI agents via MCP.

mod pix;
mod tools;

use anyhow::Result;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging to stderr (stdout is reserved for the MCP JSON-RPC transport).
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(false),
        )
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    tracing::info!("Starting PIX MCP Server v{}", env!("CARGO_PKG_VERSION"));

    // Serve the MCP protocol over stdio using the official rmcp SDK.
    let service = tools::create_server()
        .serve(stdio())
        .await
        .inspect_err(|e| {
            tracing::error!("Failed to start MCP server: {:?}", e);
        })?;

    service.waiting().await?;

    Ok(())
}
