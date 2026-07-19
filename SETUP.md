# PIX MCP Server — Setup

## Prerequisites

1. **Rust 1.88 or newer** — install from <https://rustup.rs>
2. **Microsoft PIX** — install from the Microsoft Store or
   <https://devblogs.microsoft.com/pix/download/>
3. **Visual Studio 2022 Build Tools** — install the **Desktop development with C++** workload,
   which includes the MSVC linker/toolset and Windows SDK required by Rust's Windows MSVC target
4. **Windows** — PIX and `pixtool.exe` are Windows-only

## Build

```powershell
git clone https://github.com/pozlabs/pix-mcp.git
cd pix-mcp
cargo build --locked --release
```

The server binary is produced at `target\release\pix-mcp.exe`.

## Run

```powershell
cargo run --locked --release
```

The server speaks MCP `2025-11-25` (JSON-RPC 2.0) over **stdio** using version 2.2 of the
official [`rmcp`](https://docs.rs/rmcp/2.2.0/rmcp/) SDK. Logs go to **stderr**; **stdout** is
reserved for the protocol. Set `RUST_LOG=debug` for verbose logging.

## Test with the MCP Inspector

The Inspector command requires Node.js/npm (`npx`) in addition to the build prerequisites.

```powershell
npx @modelcontextprotocol/inspector cargo run --locked --release
```

## Runtime constraints

- MCP tool calls have a process-wide concurrency limit of eight by default; configure a value in
  `1..=64` with `PIX_MCP_MAX_CONCURRENT_TOOLS`. Calls wait at most 30 seconds for a global slot.
  Foreground `pixtool` operations use a two-process pool and wait up to 30 seconds for a permit
  before their execution timeout starts. Background launches use a separate four-process pool; a
  fifth concurrent background launch fails immediately instead of waiting.
- MCP request cancellation drops the active tool future. Foreground `pixtool` operations time out
  after 10 minutes, background launches after 30 minutes, and timing captures after the requested
  duration plus 30 seconds. Timed-out processes and cancelled foreground processes are terminated.
- MCP Tasks are not advertised; calls use normal request cancellation and the bounded timeouts
  above. `pix_open_capture` launches the WinPix GUI intentionally outside the managed pools.
- Capture analysis requires Windows Developer Mode for `save-event-list`, `save-screenshot`,
  `save-resource`, `list-counters`, and `run-debug-layer`. Timing captures require elevation;
  ordinary GPU capture does not.
- User-controlled application launch is blocked when the server is elevated. This does not block
  timing capture by PID. Prefer separate elevated and non-elevated server configurations; only set
  `PIX_MCP_ALLOW_ELEVATED_LAUNCH=true` when every allowed executable is trusted.
- GPU capture `frames` defaults to pixtool's default of 1 and must be in `1..=120`.
  Timing-capture `duration_ms` defaults to 100 milliseconds and must be in `1..=600000`.
- pixtool 2603.25 cannot faithfully represent application arguments containing spaces, beginning
  with `-`/`+`, or containing quotes/control characters. Such arguments are rejected before launch;
  use the application's config/`autoexec` file or an environment variable instead.
- `pix_run_analysis` replays the capture with the D3D12 debug layer and validates playback, but
  pixtool does not export the debug-layer messages. Its issue list is therefore not a complete
  diagnostic inventory.
- Event-list CSV and screenshot PNG outputs are generated at unique temporary paths and validated
  before replacing the requested destination. Capture `.wpix` files are also generated at unique
  staging paths inside an isolated same-filesystem directory under the destination parent,
  verified as non-empty, and persisted with no-clobber semantics. Existing capture destinations
  are never overwritten, and partial outputs are cleaned up. The PNG name derived by the combined
  capture-and-analyze workflow is no-clobber and produces a warning when already present.
- `pix_get_event_list.output_path`, when provided, must end with `.csv`; other extensions are
  rejected to protect captures and unrelated files from accidental replacement.
- Capture outputs with no extension gain `.wpix`; conflicting extensions and directory paths are
  rejected. Screenshot outputs gain a final `.png` when absent. `pix_list_captures` defaults to
  100 results per page and accepts at most 500; event-list inline limits default to 50/500 for
  summary/full and are capped at 2000 rows and 1 MiB. File-backed event CSV validation is streamed
  and capped at 128 MiB. Other inline analysis reports are capped at 1 MiB, counter lists report
  truncation explicitly, and capture-directory scans accept at most 20,000 entries.
- `pix_compare_captures` compares file-size and modification metadata only. It does not establish a
  performance regression; compare equivalent event timings and GPU counters for that analysis.

## Local security policy

Use `pix-mcp` only with a trusted MCP client. Input validation and allowlists are defense in depth,
not an OS sandbox. The following environment variables configure the local boundary:

| Variable | Default | Purpose |
|----------|---------|---------|
| `PIX_MCP_CAPTURES_DIR` | process working directory | Existing directory used by `pix_list_captures` and `capture://` resources |
| `PIX_MCP_INPUT_ROOTS` | unrestricted local paths | Semicolon-separated existing roots for capture/counter inputs |
| `PIX_MCP_OUTPUT_ROOTS` | unrestricted local paths | Semicolon-separated existing roots for capture/CSV/PNG outputs |
| `PIX_MCP_EXECUTABLE_ROOTS` | unrestricted local paths | Semicolon-separated existing roots for target executables and working directories |
| `PIX_MCP_ALLOW_UNC_PATHS` | `false` | Allow UNC paths; Windows device/verbatim namespaces remain forbidden |
| `PIX_MCP_ALLOW_ELEVATED_LAUNCH` | `false` | Allow user-controlled application launch while elevated |
| `PIX_MCP_MAX_CONCURRENT_TOOLS` | `8` | Global MCP tool-call limit (`1..=64`) |

Setting `PIX_MCP_INPUT_ROOTS` or `PIX_MCP_OUTPUT_ROOTS` also implicitly includes the captures
directory. Set either variable to an empty string to allow only the captures directory for that
class. Relative artifact paths are resolved from the captures directory; relative executable and
working-directory paths use the process directory. Roots and existing paths are canonicalized; reparse-point targets must remain within an
allowed root. UNC paths require explicit opt-in, while device namespaces, alternate data streams,
reserved device names, and ambiguous Windows paths are always rejected.

Example hardened configuration:

```powershell
$env:PIX_MCP_CAPTURES_DIR = 'D:\PIX\Captures'
$env:PIX_MCP_INPUT_ROOTS = 'D:\PIX\Captures;D:\PIX\CounterExports'
$env:PIX_MCP_OUTPUT_ROOTS = ''
$env:PIX_MCP_EXECUTABLE_ROOTS = 'D:\Games\TrustedBuilds'
cargo run --locked --release
```

## Development checks

```powershell
cargo fmt --check
cargo build --locked --all-targets --all-features
cargo test --locked --all-targets --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo audit
```

If needed, install the audit command with `cargo install cargo-audit --locked`.

## Project structure

```
src/
├── main.rs            # entry point: starts the rmcp stdio server
├── security.rs        # path, privilege, diagnostic, and concurrency policy
├── pix/
│   ├── mod.rs
│   └── pixtool.rs     # pixtool.exe discovery + subprocess wrapper
└── tools/
    ├── mod.rs         # PixServer: tool routing, cancellation, elicitation, resources
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
