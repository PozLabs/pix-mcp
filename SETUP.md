# PIX MCP Server — Setup

## Prerequisites

1. **Rust toolchain** — install from <https://rustup.rs>
2. **Microsoft PIX** — install from the Microsoft Store or
   <https://devblogs.microsoft.com/pix/download/>
3. **Windows** — PIX and `pixtool.exe` are Windows-only

## Build

```powershell
git clone https://github.com/pozlabs/pix-mcp.git
cd pix-mcp
cargo build --release
```

The server binary is produced at `target\release\pix-mcp.exe`.

## Run

```powershell
cargo run --release
```

The server speaks MCP (JSON-RPC 2.0) over **stdio** using the official
[`rmcp`](https://crates.io/crates/rmcp) SDK. Logs go to **stderr**; **stdout** is reserved
for the protocol. Set `RUST_LOG=debug` for verbose logging.

## Test with the MCP Inspector

```powershell
npx @modelcontextprotocol/inspector cargo run --release
```

## Project structure

```
src/
├── main.rs            # entry point: starts the rmcp stdio server
├── pix/
│   ├── mod.rs
│   └── pixtool.rs     # pixtool.exe discovery + subprocess wrapper
└── tools/
    ├── mod.rs         # PixServer: tool router, tasks, elicitation, resources, ServerHandler
    ├── status.rs      # pix_status
    ├── launch.rs      # pix_launch, pix_launch_and_capture
    ├── capture.rs     # GPU/timing capture, list/open captures
    ├── analysis.rs    # analyze, event list (paginated), screenshot (image), counters, frame insights
    ├── workflow.rs    # pix_capture_and_analyze (one-shot launch+capture+analyze)
    └── resources.rs   # capture:// resource URIs
```

## PIX discovery

The server locates `pixtool.exe` automatically under
`C:\Program Files\Microsoft PIX\<version>\pixtool.exe` (newest version first). Override the
location with the `PIXTOOL_PATH` environment variable if PIX is installed elsewhere.
