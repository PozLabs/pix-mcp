# PIX MCP Server

A Model Context Protocol (MCP) server that enables AI agents (GitHub Copilot, Claude) to debug DirectX 12 applications using Microsoft PIX.

## Features

- **Launch applications with PIX** - Start executables with PIX attached for GPU capture
- **GPU Captures** - Capture DirectX 12 GPU work to `.wpix` files
- **Timing Captures** - Record CPU/GPU timing for performance analysis
- **Capture Analysis** - Extract event lists, counters, screenshots and run debug-layer validation
- **Structured results** - Every tool returns typed `structuredContent` with a JSON `outputSchema`
- **Capture Management** - List and open capture files

## Prerequisites

1. **Rust** - Install from [rustup.rs](https://rustup.rs)
2. **Microsoft PIX** - Install from [Microsoft Store](https://apps.microsoft.com/store/detail/pix-on-windows/9PGD9BTP9D71) or [PIX downloads](https://devblogs.microsoft.com/pix/download/)
3. **Windows SDK** - For Direct3D12 headers (comes with Visual Studio)

## Installation

```powershell
# Clone the repository
git clone https://github.com/pozlabs/pix-mcp.git
cd pix-mcp

# Build (produces target\release\pix-mcp.exe)
cargo build --release
```

## Usage with AI Agents

### GitHub Copilot (VS Code)

Add to your MCP settings (`.vscode/settings.json` or user settings):

```json
{
  "github.copilot.chat.mcpServers": {
    "pix-mcp": {
      "command": "C:\\path\\to\\pix-mcp\\target\\release\\pix-mcp.exe"
    }
  }
}
```

### Claude Desktop

Add to `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "pix-mcp": {
      "command": "C:\\path\\to\\pix-mcp\\target\\release\\pix-mcp.exe"
    }
  }
}
```

## Available Tools

### Launch & Capture

| Tool | Description |
|------|-------------|
| `pix_launch` | Launch an executable with PIX attached |
| `pix_launch_and_capture` | Launch with PIX capturing from start |
| `pix_gpu_capture` | Capture GPU frames from a running process (requires PID) |
| `pix_gpu_capture_launch` | Launch executable and capture GPU frames to file |
| `pix_timing_capture` | Record CPU/GPU timing from a running process (admin required) |
| `pix_capture_and_analyze` | **One-shot**: launch → GPU capture → frame-insights summary (+ screenshot) |

### Markers & Events

> Removed: PIX event markers (`PIXSetMarker`/`PIXBeginEvent`) must be emitted from inside
> the target application's render loop (they require a D3D12 command list/queue in the target
> process), so they cannot be driven from an external MCP server. Add markers via the
> WinPixEventRuntime in your application instead.

### Capture Management

| Tool | Description |
|------|-------------|
| `pix_list_captures` | List .wpix files in a directory |
| `pix_open_capture` | Open a capture in PIX GUI |

### Health & Analysis

| Tool | Description |
|------|-------------|
| `pix_status` | Check PIX installation and server health |
| `pix_analyze_capture` | Analyze .wpix file — extract events, counters, performance data |
| `pix_analyze_frame` | **Heuristic frame triage** — draw/dispatch/barrier counts, RT changes, top expensive events |
| `pix_get_event_list` | Extract D3D12 event list (paginated via `offset`/`limit`/`response_format`, or save full CSV) |
| `pix_list_counters` | List available performance counters (supports `filter`/`limit`) |
| `pix_run_analysis` | Run debug layer analysis, detect D3D12 errors |
| `pix_get_screenshot` | Extract the frame **recorded with the capture** as PNG (`save-screenshot`) and return it inline as an image; `depth`/`marker` options save a render target/depth buffer via replay |
| `pix_export_counters` | Parse PIX-exported counters (CSV/JSON) |
| `pix_compare_captures` | Compare two captures for regression detection |

## Protocol Features

- **Latest MCP protocol** (`2025-11-25`) via the official [`rmcp`](https://crates.io/crates/rmcp) SDK.
- **MCP Tasks** — long-running tools (captures, analysis) accept task-augmented calls
  (`tasks.requests.tools.call`), so clients can poll for deferred results and cancel.
- **Structured output** — every tool advertises a JSON `outputSchema` and returns `structuredContent`.
- **Image content** — `pix_get_screenshot` returns the rendered frame as an inline image.
- **Elicitation** — a missing `output_path` is requested interactively when the client supports
  elicitation; otherwise a clear, model-correctable tool error is returned.
- **Token-efficient** — list tools paginate and can write full data to files instead of inlining it.

## pixtool compatibility (2603.25)

Command/flag usage is matched to the installed binary — see
[`pixtool-reference.md`](pixtool-reference.md), a verbatim dump of `pixtool --help`. Notes that
affect this server:

- **Analysis needs Developer Mode.** `save-event-list`, `save-screenshot`, `save-resource`,
  `list-counters`, and `run-debug-layer` fail without Windows Developer Mode; the server detects
  this and returns actionable guidance. Capturing does not need it.
- **App arguments with spaces / leading `-`/`+` can't be passed via `--command-line`** on 2603.25.
  The capture tools still send them, but warn in the result; prefer the app's own `autoexec`/config
  file or an environment variable to select a level/mode.
- **GPU capture by PID requires launch-under-PIX** (`pix_gpu_capture` only works on a process PIX
  launched). Use `pix_gpu_capture_launch` / `pix_capture_and_analyze` for a normal game.
- **Timing capture `duration_ms` is in milliseconds** (pixtool default 100).

## MCP Resources

| Resource URI | Description |
|--------------|-------------|
| `capture://list` | List all available captures |
| `capture://{id}` | Get metadata for a specific capture |
| `capture://{id}/metadata` | Get file metadata for a capture |
| `capture://{id}/events` | Hint to use the `pix_get_event_list` tool |
| `capture://{id}/counters` | Hint to use the `pix_list_counters` tool |

## Example Workflow

**One-shot (recommended):**
```
Agent: "Debug the rendering issue in my game"

1. pix_capture_and_analyze({
     exe_path: "C:\\MyGame\\game.exe",
     output_path: "C:\\Captures\\issue.wpix"
   })
   → Launches the game, takes a GPU capture, and returns a frame-insights
     summary (draw calls, barriers, most expensive events) plus a screenshot.

2. pix_get_screenshot({ capture_path: "C:\\Captures\\issue.wpix", output_path: "C:\\Captures\\frame.png" })
   → Returns the rendered frame inline so the model can see the bug.

3. pix_open_capture({ capture_path: "C:\\Captures\\issue.wpix" })
   → Opens in the PIX GUI for deeper analysis.
```

**Step-by-step:**
```
1. pix_gpu_capture_launch({ 
     exe_path: "C:\\MyGame\\game.exe",
     output_path: "C:\\Captures\\issue.wpix"
   })
   → Launches game and captures GPU frames

2. pix_analyze_frame({ capture_path: "C:\\Captures\\issue.wpix" })
   → Heuristic triage of the captured frame

3. pix_open_capture({ capture_path: "C:\\Captures\\issue.wpix" })
   → Opens in PIX for analysis
```

> **GPU capture requires launching under PIX.** PIX can only take a GPU capture of a
> process that PIX itself launched, so the single-shot tools above
> (`pix_capture_and_analyze`, `pix_gpu_capture_launch`) are the reliable path.
> `pix_gpu_capture` (attach by PID) only works on a process PIX already launched —
> attaching to an independently-started game fails with `PIXTOOL17 - Process not
> launched for GPU Capture`. `pix_launch` returns pixtool's launcher PID, not the
> game's, and does not leave a process you can later capture by PID.
>
> Tip: pass `frames: 1` to the capture tools to bound the capture and let the
> launched app close promptly.

## Environment Variables

| Variable | Description |
|----------|-------------|
| `PIXTOOL_PATH` | Override path to pixtool.exe |
| `RUST_LOG` | Set logging level (e.g., `debug`, `info`) |

## Architecture

```
┌─────────────────────────────────────────────────┐
│              AI Agent (Copilot/Claude)          │
└──────────────────────┬──────────────────────────┘
                       │ JSON-RPC 2.0 (stdio)
                       ▼
┌─────────────────────────────────────────────────┐
│              PIX MCP Server (Rust)              │
│  ┌──────────────┐  ┌──────────────────────────┐ │
│  │ rmcp SDK     │  │ pixtool.exe wrapper      │ │
│  └──────────────┘  └──────────────────────────┘ │
│  ┌──────────────────────────────────────────┐   │
│  │        pixtool.exe Subprocess            │   │
│  └──────────────────────────────────────────┘   │
└──────────────────────┬──────────────────────────┘
                       │
                       ▼
┌─────────────────────────────────────────────────┐
│     PIX Runtime + Target DirectX 12 App         │
└─────────────────────────────────────────────────┘
```

## Development

```powershell
cargo build      # debug build
cargo test       # run tests
cargo clippy     # lints
cargo run        # run the server over stdio
```

See [SETUP.md](SETUP.md) for prerequisites and how to test with the MCP Inspector.

## Contributing

Issues and pull requests are welcome. Before opening a PR, please run `cargo fmt`, `cargo clippy`, and `cargo test`.

## License

Licensed under the [MIT License](LICENSE). © 2026 Alessandro Pozone.
