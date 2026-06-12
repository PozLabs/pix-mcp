# `pixtool.exe` — complete command reference (PIX on Windows 2603.25)

> **Source of truth: the installed binary, not a blog.** Every command, description,
> argument and sub-option below is a verbatim dump of `pixtool --help` and
> `pixtool --help <command>` from
> `C:\Program Files\Microsoft PIX\2603.25\pixtool.exe`, captured 2026-06-12.
> Regenerate after a PIX update — option sets change between versions.
>
> Reproduce:
> ```powershell
> $pt = 'C:\Program Files\Microsoft PIX\2603.25\pixtool.exe'
> & $pt --help
> foreach ($c in @('launch','launch-app','set-gpu-capture-parameters','take-capture',
>   'programmatic-capture','save-capture','save-all-captures','open-capture','list-counters',
>   'save-event-list','run-debug-layer','collect-occupancy','save-high-frequency-counters',
>   'save-screenshot','save-resource','begin-recapture','end-recapture','recapture-single-playback',
>   'perform-single-playback','recapture-region','take-new-timing-capture','attach','export-to-cpp',
>   'upgrade-gpu-capture')) { "### $c"; & $pt --help $c }
> ```

## Invocation model

```
pixtool [--help [<command>]] [...] <command> [<command>] [...]
```

- **Multiple commands per line**, executed left-to-right in one process — this is how
  you chain `launch … take-capture save-capture …` or
  `open-capture … save-event-list …`.
- A **caret `^`** splits a long command line across multiple lines (cmd.exe).
- Exit code note: several read-only/help paths return a non-zero/`0xFFFFFFFF` exit code
  even on success — don't treat a non-zero exit as failure for `--help`/analysis verbs;
  parse stdout.

### Global options (before any command)

| Option | Meaning |
|--------|---------|
| `--help` | Show the top-level help message. |
| `--help <command>` | Show help for a specific command. |
| `--output=<level>` | Output verbosity: `quiet`, `trace` (default), `engine`, `verbose`. |
| `--log=<level>` | Log-file verbosity: same levels + `off`. Default `verbose`. |
| `--log-file=<filename>` | Log-file path. Default `pixtool.log` in `%TEMP%`. |

### Official example chains (from the binary)

```
pixtool launch d3d12hellotriangle.exe take-capture --open save-capture foo.wpix
pixtool open-capture foo.wpix save-event-list eventlist.txt --counters=*
pixtool open-capture foo.wpix list-counters
pixtool open-capture --remote=192.168.1.1 foo.wpix save-event-list eventlist.txt --counter-groups=D3D*
```

## Command index (24 verbs)

| # | Command | One-line |
|---|---------|----------|
| 1 | [`launch`](#launch) | Launch an executable for GPU capture |
| 2 | [`launch-app`](#launch-app) | Launch a UWP app for GPU capture |
| 3 | [`set-gpu-capture-parameters`](#set-gpu-capture-parameters) | Set parameters for all subsequent captures |
| 4 | [`take-capture`](#take-capture) | Take a GPU capture of the currently launched app |
| 5 | [`programmatic-capture`](#programmatic-capture) | Wait for an app-triggered programmatic capture |
| 6 | [`save-capture`](#save-capture) | Save the currently open capture to a file |
| 7 | [`save-all-captures`](#save-all-captures) | Save all recently taken captures to a directory |
| 8 | [`open-capture`](#open-capture) | Open a `.wpix` GPU capture for analysis |
| 9 | [`list-counters`](#list-counters) | List all available counters |
| 10 | [`save-event-list`](#save-event-list) | Save the event list as CSV |
| 11 | [`run-debug-layer`](#run-debug-layer) | Force a playback with the D3D12 debug layer |
| 12 | [`collect-occupancy`](#collect-occupancy) | Collect GPU occupancy data |
| 13 | [`save-high-frequency-counters`](#save-high-frequency-counters) | Save High Frequency Counters CSV |
| 14 | [`save-screenshot`](#save-screenshot) | Save the capture's screenshot as PNG |
| 15 | [`save-resource`](#save-resource) | Save a resource from a given draw call |
| 16 | [`begin-recapture`](#begin-recapture) | Begin a capture of pixtool itself |
| 17 | [`end-recapture`](#end-recapture) | End a recapture |
| 18 | [`recapture-single-playback`](#recapture-single-playback) | Capture a playback of the open file |
| 19 | [`perform-single-playback`](#perform-single-playback) | Perform one playback of the open file |
| 20 | [`recapture-region`](#recapture-region) | Recapture a GlobalID region of the open file |
| 21 | [`take-new-timing-capture`](#take-new-timing-capture) | Take a Timing Capture |
| 22 | [`attach`](#attach) | Attach to a running process |
| 23 | [`export-to-cpp`](#export-to-cpp) | Export the captured frame as a C++ project |
| 24 | [`upgrade-gpu-capture`](#upgrade-gpu-capture) | Upgrade a capture file to the latest format |

> **Not in pixtool:** *Pixel History* and *Debug Pixel* are **GUI-only** (WinPix.exe) —
> they are not pixtool verbs. The closest headless equivalents are
> [`save-resource --global-id`](#save-resource) (dump the RTV/depth bound at a specific
> event) and [`save-event-list`](#save-event-list) (per-event counters).

---

## Capture (launch / acquire)

### <a id="launch"></a>1. `launch`
```
launch <exe> [...]
Launches an executable for GPU capture

    --command-line=<cl>                 Sets the command line parameters to pass to the executable.
    --working-directory=<dir>           Sets the working directory
                                        Default: the directory containing the exe
    --setenv=<envVar>                   Sets the environment variables of the executable.
                                        envVar format: <VARNAME=VARVALUE>
                                        The first instance of "=" separates name and value.
                                        Specify multiple times for multiple variables, e.g.
                                        --setenv="VAR1=VALUE1" --setenv="VAR2=VALUE2"
    --remote=<machine name>             Specifies the remote capture machine. Default: localhost
    --timing                            Launch the app for Timing Capture.
    --force11on12                       Forces PIX to use D3D11on12.
    --captureFromStart                  Forces PIX to start capturing before the app starts running.
```
> ⚠️ **2603.25 parsing caveat (verified):** `--command-line=<value>` is rejected when the
> value **contains a space** (`foobar` parses; `foo bar` → `Unknown option`) or starts
> with `-`/`+`. Multi-token game args (`-windowed +runworld worlds\...\c01`) cannot be
> passed this way — drive the world via `autoexec.cfg` instead, or `--setenv`.

### <a id="launch-app"></a>2. `launch-app`
```
launch-app <package> <application> [...]
Launches a UWP app for GPU capture

    <package>          The Package Full Name of the package containing the app.
    <application>      The Application ID of the app.
    --remote=<machine name>   Specifies the remote capture machine. Default: localhost
    --timing           Launch the app for Timing Capture.
    --force11on12      Forces PIX to use D3D11on12. Mutually exclusive with --ignoreD3D11.
    --ignoreD3D11      Forces PIX to ignore D3D11 API calls. Mutually exclusive with --force11on12.

Use the PIX UI to determine values to pass to this command. Under PC Connection /
Select Target Process / Launch UWP, right-click on an app and choose 'Copy pixtool
Launch Command' to copy the command to the clipboard.
```

### <a id="set-gpu-capture-parameters"></a>3. `set-gpu-capture-parameters`
```
set-gpu-capture-parameters [--frames=<n>] [...]
Sets the parameters for all subsequent captures

    --frames=<n>            Number of frames to capture. Default: 1
    --capture-key=<F1-F12>  Sets the manual capture key (F1-F12 only). Default: F11
    --winml                 Uses Windows ML Work as the frame delimiter instead of Present-to-Present.
    --remote=<machine name> Remote capture machine to configure. Default: localhost
```

### <a id="take-capture"></a>4. `take-capture`
```
take-capture [--open] [...]
Take a GPU capture of the currently launched app

    --open          Opens this capture on the target machine after taking it.
    --winml         Uses Windows ML Work as the frame delimiter instead of Present-to-Present.
    --frames=<n>    Number of frames to capture. Default: 1, or as set by set-gpu-capture-parameters.
```
> Takes the **next presented frame(s)** after the verb runs — for a `launch … take-capture`
> chain that is the first rendered frame (e.g. a title/loading screen), **not** a specific
> gameplay frame. For a targeted frame use [`programmatic-capture`](#programmatic-capture)
> with the app's own PIX trigger.

### <a id="programmatic-capture"></a>5. `programmatic-capture`
```
programmatic-capture [--open] [--until-exit]
Wait for a programmatic capture to be triggered by the currently launched app

    --open         Opens the programmatic capture on the target machine after it is triggered.
    --until-exit   Captures all programmatic captures from the app into multiple capture files.
                   Use save-all-captures to save these files to a specific directory.
```
> Pairs with the engine's in-app PIX trigger (`PIXBeginCapture`/F12 path in
> `d3drs_core_sync.cpp`) so the captured frame is the exact one the engine asks for —
> the right tool for a deterministic c01 frame.

### <a id="save-capture"></a>6. `save-capture`
```
save-capture <filename>
Saves the currently open capture to the given filename
```

### <a id="save-all-captures"></a>7. `save-all-captures`
```
save-all-captures <directory>
Saves all recently taken captures to the specified directory
(those taken with take-capture or programmatic-capture)

Example:
  pixtool launch d3d12hellotriangle.exe take-capture take-capture save-all-captures c:\output
  pixtool launch d3d12hellotriangle.exe programmatic-capture --until-exit save-all-captures c:\output
```

### <a id="attach"></a>22. `attach`
```
attach <targetProcessId>
Attach to the specified running process

    <targetProcessId>          Target Process Id.
    --remote=<machine name>    Specifies the remote capture machine. Default: localhost
```
> ⚠️ **GPU capture requires launch-under-PIX.** Attaching to a process that was started
> normally and then running `take-capture` fails with
> `PIXTOOL17 — Process not launched for GPU Capture`. `attach` is usable for processes
> PIX itself launched.

---

## Analysis (open + extract) — requires Windows **Developer Mode**

> `save-event-list`, `save-resource`, `save-screenshot` and other playback verbs fail with
> `E_PIX_FEATURE_REQUIRES_DEVELOPER_MODE` unless Developer Mode is enabled
> (Settings → Developer Mode, or admin:
> `reg add HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock /v
> AllowDevelopmentWithoutDevLicense /t REG_DWORD /d 1`). Capture does **not** need it.

### <a id="open-capture"></a>8. `open-capture`
```
open-capture <filename> [...]
Opens a Windows GPU capture wpix file

    --remote=<machine name>                        Remote analysis machine. Default: localhost
    --use-replay-time-executeindirect-buffers      Use replay-time ExecuteIndirect argument buffers
                                                   instead of capture-time buffers.
    --disable-gpu-plugins                          Do not load GPU Plugins while the capture is open.
    --enable-recreate-at-gpuva                     Recreate heaps and buffer resources at capture-time
                                                   GPUVAs if the replay device supports it.
    --enable-application-specific-driver-state     Set application specific driver workarounds during replay.
    --force-set-application-specific-driver-state  Force set those workarounds irrespective of device/driver mismatch.
```

### <a id="list-counters"></a>9. `list-counters`
```
list-counters
Lists all the available counters
```

### <a id="save-event-list"></a>10. `save-event-list`
```
save-event-list <filename> [...]
Saves the event list in CSV format

    --counters=<pattern>        Includes counters matching the pattern. Queue ID, Name and
                                Global ID are always included. Repeatable. '*' matches any
                                sequence of characters.
    --counter-groups=<pattern>  Includes all counters inside matching counter groups.
                                e.g. "D3D:*" includes all the D3D counters.
    --queue-name=<queue name>   The name of the queue to use for the event list.
```
> The per-draw table (with the always-present **Global ID** column) is how you find the
> Global ID of a draw to feed [`save-resource --global-id`](#save-resource).

### <a id="run-debug-layer"></a>11. `run-debug-layer`
```
run-debug-layer
Forces a playback with the debug layer enabled
This command doesn't generate any output, although it does validate that playback succeeds.
```

### <a id="collect-occupancy"></a>12. `collect-occupancy`
```
collect-occupancy
Makes pixtool collect GPU occupancy data for the capture
This command doesn't generate any output, although it does validate that playback succeeds.
```

### <a id="save-high-frequency-counters"></a>13. `save-high-frequency-counters`
```
save-high-frequency-counters <filename> [...]
Collects and saves High Frequency Counters data in CSV format

    --counters=<pattern>   Includes counters matching the pattern. Repeatable. '*' wildcards.
    --merge                Merges the timestamps to a single column and coalesces the counter data.
```

### <a id="save-screenshot"></a>14. `save-screenshot`
```
save-screenshot <filename>
Saves the capture's screenshot as a PNG file
```

### <a id="save-resource"></a>15. `save-resource`
```
save-resource <filename> [...]
Saves a resource from a given draw call

Resource Selection:
    --rtv=<RenderTargetView index>  Which RTV (in an API call with multiple RTVs) to save. Default: 0
    --depth                         Save a visual representation of a depth buffer. PNG only.

Event Selection:
    --global-id=<Global ID>         Save the resource from the event with this Global ID.
    --marker=<name>                 Save resource from the last child with a bound instance,
                                    under the named PIX marker.

Defaults: RTV 0; the current contents of the resource in the LAST event that has it bound.
Use --global-id or --marker to choose the event. File format is set by the extension.
When a marker appears in multiple queues, the behavior is undefined.
```
> ★ **Headless per-draw output for 109-1:** `--global-id=<id>` dumps the bound render
> target (or `--depth`) as it stood at that specific draw → dump the draw that colours the
> dark wall crop and read its RGB, the CLI analogue of GUI Pixel History.

---

## Recapture / replay / region

### <a id="begin-recapture"></a>16. `begin-recapture`
```
begin-recapture
Begins a capture of pixtool itself
pixtool must be launched for GPU capture from either PIX or another instance of pixtool;
otherwise this command will fail.
```

### <a id="end-recapture"></a>17. `end-recapture`
```
end-recapture
Ends a recapture
begin-recapture must precede this command.
```

### <a id="recapture-single-playback"></a>18. `recapture-single-playback`
```
recapture-single-playback [...]
Performs capture of a playback of the currently open file

    --include-recreation   Captures the recreation events executed by replay, rather than excluding them.
    --expand-execute       Expands the contents of ExecuteIndirect argument buffers during replay.
```

### <a id="perform-single-playback"></a>19. `perform-single-playback`
```
perform-single-playback [...]
Performs one single playback of the currently open file

    --do-not-expand-executeindirect  Do not expand ExecuteIndirect argument buffers into the
                                     parent command list during replay.
    --loop                           Repeatedly performs the single playback until the process is terminated.
    --loop-count                     How many times to loop. Default 0 = loop indefinitely.
                                     Only valid with --loop.
    --time-cpu                       Measure the time taken to record the command list during playback.
                                     Writes a CSV in the CWD with per-API record times.
    --measure-cycles                 Measure CPU time in cycles instead of milliseconds.
                                     Only valid with --time-cpu.
```

### <a id="recapture-region"></a>20. `recapture-region`
```
recapture-region <outputFilePath> [...]
Recaptures a region of the currently open capture file

    --start   First GlobalID to include (inclusive)
    --end     Last GlobalID to include (inclusive)

Example:
  pixtool open-capture c:\mydir\foo.wpix recapture-region c:\mydir\recaptured_foo.wpix --start=4 --end=5
```

---

## Timing capture

### <a id="take-new-timing-capture"></a>21. `take-new-timing-capture`
```
take-new-timing-capture <filename> [--duration=<n>] [...]
Take a New Timing Capture of the current app and save it

    filename            Target capture file path. The target directory must exist.
    --duration=<n>      Capture duration in milliseconds. Default: 100
    --sampleRate=<n>    CPU sampling rate (samples/sec). Default: 1000, Min: 1, Max: 8000.
                        No effect if --noCpuSamples.
    --noCpuSamples      Exclude CPU samples from the capture.
    --noCallstacks      Do not capture callstacks on context switches.
    --noGpuTimings      Exclude GPU timings from the capture.
```
> Timing captures require **administrator** privileges.

---

## Export / maintenance

### <a id="export-to-cpp"></a>23. `export-to-cpp`
```
export-to-cpp <directory> [--force] [--use-winpixeventruntime] [...]
Exports PIX captured frame as C++ project

    --force                                     Overwrites files with same name.
    --use-winpixeventruntime                    Acknowledge/accept the WinPixEventRuntime license.
                                                https://www.nuget.org/packages/WinPixEventRuntime
    --use-agilitySdk                            Acknowledge/accept the DirectX 12 Agility SDK license.
                                                https://www.nuget.org/packages/Microsoft.Direct3D.D3D12
    --use-replay-time-executeindirect-buffers   Replay ExecuteIndirects using replay-time argument
                                                buffers (default uses copies of capture-time buffers).

Example:
  pixtool open-capture d3d12hellotriangle.exe export-to-cpp --use-winpixeventruntime --use-agilitySdk C:\exportedprojects
  pixtool launch C:\...\D3D12HelloTriangle.exe take-capture --open export-to-cpp --force --use-replay-time-executeindirect-buffers C:\exportedprojects
```

### <a id="upgrade-gpu-capture"></a>24. `upgrade-gpu-capture`
```
upgrade-gpu-capture <sourceCapturePath> [--dest=<destCapturePath>]
Upgrades a GPU capture file to the latest format

    --dest=<destCapturePath>   Destination for the upgraded capture.
                               If omitted, sourceCapturePath is overwritten.
```

---

## How this maps to the BUG-109 open points

| Need | Verb chain |
|------|-----------|
| Capture a deterministic c01 frame | engine F12 trigger + `launch … programmatic-capture --open save-capture c01.wpix` (avoid `--command-line` arg bug: put `runworld` in `autoexec.cfg`) |
| List every draw + Global IDs | `open-capture c01.wpix save-event-list events.csv --counters=*` |
| Dump the RGB a specific draw wrote (109-1 dark crop) | `open-capture c01.wpix save-resource draw.png --global-id=<id>` (add `--rtv=N` / `--depth` as needed) |
| Visual check / GUI Pixel History & Debug Pixel | open in WinPix.exe (GUI-only; not a pixtool verb) |
| Count un-fused LightmapTex (109-3) | `save-event-list` + inspect per-draw bound SRVs, or per-draw `save-resource` diffs |
