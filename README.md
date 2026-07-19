# PIX MCP Server

A Model Context Protocol (MCP) server that enables AI agents (GitHub Copilot, Claude) to debug DirectX 12 applications using Microsoft PIX.

## Features

- **Launch applications with PIX** - Start executables with PIX attached for GPU capture
- **GPU Captures** - Capture DirectX 12 GPU work to `.wpix` files
- **Timing Captures** - Record CPU/GPU timing for performance analysis
- **Capture Analysis** - Extract event lists, counters, screenshots and validate replay with the D3D12 debug layer
- **Structured results** - Every tool returns typed `structuredContent` with a JSON `outputSchema`
- **Capture Management** - List and open capture files

## Prerequisites

1. **Windows** - PIX and `pixtool.exe` are Windows-only
2. **Rust 1.88 or newer** - Install from [rustup.rs](https://rustup.rs)
3. **Microsoft PIX** - Install from [PIX downloads](https://devblogs.microsoft.com/pix/download/)
4. **Visual Studio 2022 Build Tools** - Install the **Desktop development with C++** workload;
   it provides the MSVC linker/toolset and Windows SDK required by Rust's Windows MSVC target

## Installation

```powershell
# Clone the repository
git clone https://github.com/pozlabs/pix-mcp.git
cd pix-mcp

# Build (produces target\release\pix-mcp.exe)
cargo build --locked --release
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
| `pix_list_captures` | List .wpix files with `offset`/`limit` pagination |
| `pix_open_capture` | Open a capture in PIX GUI |

### Health & Analysis

| Tool | Description |
|------|-------------|
| `pix_status` | Check PIX installation and server health |
| `pix_analyze_capture` | Analyze .wpix file — extract events, counters, performance data |
| `pix_analyze_frame` | **Heuristic frame triage** — draw/dispatch/barrier counts, RT changes, top expensive events |
| `pix_get_event_list` | Extract D3D12 events (`offset`/`limit`; `response_format` selects the 50/500 default; maximum 2000 rows and 1 MiB inline), or save the full list when `output_path` ends with `.csv` |
| `pix_list_counters` | List available performance counters (`filter`/`limit`; reports `truncated` when bounded) |
| `pix_run_analysis` | Replay with the D3D12 debug layer to validate playback; pixtool does not export the debug-layer messages |
| `pix_get_screenshot` | Extract the frame **recorded with the capture** as PNG (`save-screenshot`) and return it inline as an image; `depth`/`marker` options save a render target/depth buffer via replay |
| `pix_export_counters` | Parse PIX-exported counters (CSV/JSON) |
| `pix_compare_captures` | Compare file-size and modification metadata for two captures (not a performance-regression analysis) |

## Protocol Features

- **MCP `2025-11-25`** via version 2.2 of the official [`rmcp`](https://docs.rs/rmcp/2.2.0/rmcp/) SDK.
- **Cancellation-aware** — MCP cancellation drops the active tool future, terminates managed
  `pixtool` process trees, and cleans up staged artifacts.
- **Direct calls** — MCP Tasks are not advertised; long-running calls use normal request
  cancellation and the server's bounded execution timeouts.
- **Progress notifications** — long PIX capture/replay/export operations emit monotonic,
  rate-limited `notifications/progress` updates when the request supplies a `progressToken`.
  Delivery failures never fail the underlying operation.
- **Structured output** — every tool advertises a JSON `outputSchema` and returns `structuredContent`.
- **Image content** — `pix_get_screenshot` returns the rendered frame as an inline image.
- **Artifact links** — newly written `.wpix`, PNG, and CSV outputs are returned as annotated,
  session-local `artifact://` resource links. Small text/images are directly readable; large or
  binary captures resolve to bounded JSON descriptors.
- **Elicitation** — when a tool requires a destination, a missing `output_path` is requested
  interactively if the client supports elicitation; otherwise a clear, model-correctable tool
  error is returned. (`pix_get_event_list` can omit it to receive inline rows.)
- **Token-efficient** — `pix_get_event_list` paginates inline rows and can write the full list to
  CSV; `pix_list_captures` defaults to 100 rows (maximum 500), and `pix_list_counters` supports
  filtering and a bounded result limit.

## pixtool compatibility (2603.25)

Command/flag usage is matched to the installed binary — see
[`pixtool-reference.md`](pixtool-reference.md), a verbatim dump of `pixtool --help`. Notes that
affect this server:

- **Analysis needs Developer Mode.** `save-event-list`, `save-screenshot`, `save-resource`,
  `list-counters`, and `run-debug-layer` fail without Windows Developer Mode; the server detects
  this and returns actionable guidance. Capturing does not need it.
- **Application arguments are validated before launch.** pixtool 2603.25 cannot faithfully
  represent arguments containing spaces, values beginning with `-` or `+`, quotes, or control
  characters through `--command-line`; the server rejects them instead of launching the target
  with altered arguments. Use the application's config/`autoexec` file or an environment variable
  to select a level or mode. In practice, only one non-empty, unprefixed token is safely supported.
- **GPU capture by PID requires launch-under-PIX** (`pix_gpu_capture` only works on a process PIX
  launched). Use `pix_gpu_capture_launch` / `pix_capture_and_analyze` for a normal game.
- **Capture bounds are validated.** `frames` defaults to pixtool's default of 1 and accepts
  `1..=120`; timing-capture `duration_ms` defaults to 100 milliseconds and accepts `1..=600000`.
- **`pix_run_analysis` validates replay, not the complete diagnostic stream.** The
  `run-debug-layer` verb replays with the D3D12 debug layer but does not export its messages, so an
  empty issue list must not be interpreted as proof that the debug layer emitted no diagnostics.
- **Processes are bounded.** MCP tool calls use a global eight-request limit by default
  (`PIX_MCP_MAX_CONCURRENT_TOOLS` can set `1..=64`) and wait at most 30 seconds for a slot.
  Foreground operations use a two-process pool and wait up to 30 seconds
  for capacity before their execution timeout starts. Background launches use a separate
  four-process pool; a fifth concurrent
  background launch fails immediately instead of waiting. Foreground operations time out after
  10 minutes, background launches after 30 minutes, and timing captures after their requested
  duration plus 30 seconds. Timed-out processes and cancelled foreground processes are terminated.
- **Analysis outputs are staged safely.** Event-list CSV and screenshot PNG outputs are written to
  isolated temporary directories, parsed/decoded to validate them, and only then replace the
  requested destination. New `.wpix` captures are likewise written to isolated same-filesystem paths,
  verified as non-empty, and persisted with no-clobber semantics. Existing capture destinations are
  never overwritten, and partial temporary files are cleaned up. The screenshot path derived by
  `pix_capture_and_analyze` is also no-clobber; an existing PNG becomes a non-fatal warning.
- **Event-list file output is type-safe.** `pix_get_event_list.output_path` must end with `.csv`;
  other extensions are rejected to avoid overwriting a capture or unrelated file. File-backed CSV
  validation is streamed and capped at 128 MiB.
- **Capture output paths are type-safe.** A missing extension is normalized to `.wpix`; a
  conflicting extension or directory path is rejected. Screenshot paths gain a final `.png` when
  it is absent.
- **Responses and scans are bounded.** Inline event pages and analysis reports are capped at 1 MiB;
  counter lists expose `truncated` when their item/byte budget is reached. Capture-directory scans
  reject directories with more than 20,000 entries.

## Trust Model

Run this server only for a trusted MCP client. The tools inherit the server user's local permissions
and this policy layer is defense in depth, not an OS sandbox. Windows device/verbatim namespaces,
alternate data streams, reserved device names, ambiguous drive-relative paths, and UNC paths are
rejected by default. Existing paths and output parents are canonicalized before allowlist checks, so
junction/symlink targets cannot escape a configured root.

For least privilege, configure `PIX_MCP_INPUT_ROOTS`, `PIX_MCP_OUTPUT_ROOTS`, and
`PIX_MCP_EXECUTABLE_ROOTS`. When an input/output root variable is present, the captures directory is
implicitly allowed; an empty input/output value therefore restricts that class to the captures
directory alone. Unset root variables retain the previous local-path behavior for compatibility.
Run normal launch/GPU-capture workflows non-elevated. An elevated server can perform timing captures
by PID, but refuses user-controlled application launches unless the operator explicitly sets
`PIX_MCP_ALLOW_ELEVATED_LAUNCH=true`.

## MCP Resources

`resources/list` is a paginated, deterministically ordered catalog containing `capture://list`
and the concrete captures in `PIX_MCP_CAPTURES_DIR`. Entries include `size`, assistant audience,
priority, and last-modified annotations. Cursors are tied to a catalog generation and expire
cleanly when files change. Filesystem watching is not enabled, so the server deliberately does not
advertise `resources.listChanged`.

| Resource URI | Description |
|--------------|-------------|
| `capture://list` | List up to 500 captures in `PIX_MCP_CAPTURES_DIR` (or the server working directory when unset); `directory`, `total_count`, and `truncated` are returned |
| `capture://{id}` | Get metadata for a specific capture |
| `capture://{id}/metadata` | Get file metadata for a capture |
| `capture://{id}/events` | Hint to use the `pix_get_event_list` tool |
| `capture://{id}/counters` | Hint to use the `pix_list_counters` tool |
| `artifact://local/{id}` | Session-local output returned by a successful tool; small artifacts are readable and large/binary artifacts return bounded descriptors |

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
> Tip: `frames` already defaults to 1, matching pixtool. Increase it only when a
> multi-frame capture is intentional (maximum 120).

## Environment Variables

| Variable | Description |
|----------|-------------|
| `PIXTOOL_PATH` | Override path to pixtool.exe |
| `PIX_MCP_CAPTURES_DIR` | Existing default directory for capture listing/resources (defaults to the server working directory) |
| `PIX_MCP_INPUT_ROOTS` | Semicolon-separated existing directories allowed for capture/counter reads; when set, the captures directory is also allowed |
| `PIX_MCP_OUTPUT_ROOTS` | Semicolon-separated existing directories allowed for `.wpix`, CSV, and PNG outputs; when set, the captures directory is also allowed |
| `PIX_MCP_EXECUTABLE_ROOTS` | Semicolon-separated existing directories allowed for target executables and working directories |
| `PIX_MCP_ALLOW_UNC_PATHS` | Opt in to UNC paths (`false` by default); device/verbatim namespaces remain forbidden |
| `PIX_MCP_ALLOW_ELEVATED_LAUNCH` | Allow user-selected application launches from an elevated server (`false` by default) |
| `PIX_MCP_MAX_CONCURRENT_TOOLS` | Global concurrent MCP tool-call limit (`8` by default, range `1..=64`) |
| `RUST_LOG` | Set logging level (e.g., `debug`, `info`) |

Relative capture, counter, CSV, and PNG paths are resolved against `PIX_MCP_CAPTURES_DIR`.
Relative executable and working-directory paths remain relative to the server process directory.

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
cargo fmt --check
cargo build --locked --all-targets --all-features
cargo test --locked --all-targets --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo audit
cargo run --locked --all-features # run the server over stdio
```

Install the audit subcommand once with `cargo install cargo-audit --locked` if it is not already
available.

See [SETUP.md](SETUP.md) for prerequisites and how to test with the MCP Inspector.

## Contributing

Issues and pull requests are welcome. Before opening a PR, run the formatting, build, test, Clippy,
and audit commands above.

## License

Licensed under the [MIT License](LICENSE). © 2026 Alessandro Pozone.
