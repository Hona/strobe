# strobe

Measure and explain performance: a web page, an Electron app, a Bun or Node process, or any process. strobe records everything on one clock, refuses measurements it cannot defend, and gives you, or your agent, a verdict with handles to drill into. It knows no product: what an app under test needs (how to build, serve or launch it in isolation) lives in that app's own `strobe.json`.

> Pre-release. Windows x64, macOS (arm64, x64) and Linux (x64, arm64). The design is in [SPEC.md](SPEC.md); what is built is listed in the skill (`skill/SKILL.md`) and `strobe --help`.

Install it per project, so everyone who runs the project's benches runs the same strobe:

```sh
bun add -d @hona/strobe       # the strobe command and the bench file types
bunx strobe up                # once per machine: engine, daemon, MCP and the skill
```

## What each OS measures

Pages, Electron apps, `script()` targets, statistics and reports work the same everywhere. What strobe reads from the OS differs: each cell is the OS's own mechanism, and a metric strobe cannot defend is refused with the reason, never printed as 0 (SPEC §13.6 has the detail).

| | Windows | Linux | macOS |
|---|---|---|---|
| process and thread CPU | exact (cycle counters) | exact (CPU clocks, `schedstat`) | exact (Mach time) |
| `cpu.wakes` | per thread | per thread | per process only (no per-thread count without root) |
| IO | all bytes | all bytes | disk bytes only |
| GPU per process | D3DKMT engines and VRAM | DRM client stats (amdgpu, Intel, …); NVIDIA proprietary: unknown | GPU time per process; VRAM refused (unified memory) |
| pinning, power plan | CCD pinning; High performance plan | `sched_setaffinity`; `performance` governor as root | none (no API); Low Power Mode reported |
| windows | Win32 | X11 (native Wayland windows are invisible to other apps) | CoreGraphics; moving a window needs Accessibility |
| launch `painted`/`settled` | screen pixels | X11 window pixels | needs Screen Recording, else refused |
| frames on screen (OS) | ETW present events | X Present completions: X11 on a real display without a compositor (Xvfb, compositors, Wayland: refused); screen times only, so `frames.interval.*` and `frames.dropped*` are refused | ScreenCaptureKit frame times of the app's windows (needs Screen Recording); screen times only, as Linux |
| drive a native app (`page`) | UI Automation; window messages posted to the app (never takes focus) | AT-SPI; XTest (the real input path; keys need the app focused, strobe activates it) | AX API (needs Accessibility); Quartz events (keys posted to the app's process, which strobe brings to the front; clicks and wheel at the point through the session) |
| keep awake during blocks | `SetThreadExecutionState` | logind `idle` inhibitor; screen saver inhibit where the desktop has it | IOKit power assertions |
| native stacks | ETW through a helper (one UAC) | `perf_event_open`, DWARF unwinding (needs `perf_event_paranoid` ≤ 2 or CAP_PERFMON) | `/usr/bin/sample` (untimed, waiting threads dropped) |
| isolated native and Electron launches | full | `HOME`/`XDG_*` | `HOME`; a Cocoa app runs only if its `~/Library` data does not exist yet |

Permissions strobe never asks for by itself: on macOS, Accessibility (to drive a native app and place its window) and Screen Recording (its frames and pixels), both for the app strobe runs under (Terminal, your agent's app, or a remote shell's daemon: `strobe status` names it); on Linux, `kernel.perf_event_paranoid=2` or CAP_PERFMON for native stacks, and the session's accessibility bus (at-spi2-core) to drive native apps.

```
strobe bench perf/timeline.bench.ts --base origin/dev --head .   # did my change help? (and why)
strobe record app:desktop --for 10s                              # what is the installed app doing?
strobe query rec_3f9a hotspots --by package                      # where did the CPU time go?
strobe query rec_3f9a layout                                     # which line forced a layout?
strobe record app:desktop --probes memory --for 60s              # is it leaking? then: query <rec> leaks
strobe report experiment exp_8c21 --open                         # a page to share
strobe top                                                       # per-core, per-process, per-GPU-engine
```

A benchmark is a small TypeScript file, like BenchmarkDotNet:

```ts
import { bench, web } from "@hona/strobe"

export default bench({
  targets: { web: web({ url: "/" }) },
  cases: {
    scroll: {
      primary: "frames.presented.p95",
      async run({ page, phase }) {
        await phase("down", () => page.scroll("[data-timeline]", { by: 4000 }))
        await phase("up", () => page.scroll("[data-timeline]", { by: -4000 }))
      },
    },
  },
})
```

Or ask your agent: *"did this branch make scrolling faster?"*

In a pull request, the evidence is strobe's block and the words are yours:

```sh
strobe report experiment exp_9b3b26 --format pr --update-pr 1229   # replace (or add) the block in PR #1229's body
```

The block starts with a "generated by strobe" line and ends with an "end of strobe report" line, between two HTML comments that carry a hash of its content, so a hand edit shows. Rendered, it reads like this:

> **`perf/idle.bench.ts`** (`exp_9b3b26`) · **2 of 2 series better**
>
> | Series · primary | Base → head | Change [CI] | Verdict | Interval (±100% scale) |
> |:---|---:|---:|:---|:---|
> | **window-closed · tray**<br>`cpu.work` | 7.05 → 3.90 ms | −44% <sub>[−54% … −32%]</sub> | **less** <sub>8/8 pairs</sub> | `·····┤●├··│··········` |
> | **window-closed-memory · tray**<br>`memory.app.private` | 229 → 50 MB | −78% <sub>[−78% … −77%]</sub> | **less** <sub>8/8 pairs</sub> | `··●·······│··········` |

then one alert for what the numbers compare with (a locked session, background load), and per series a chart of the pairs and folded drawers: context metrics, every pair, attribution (threads and memory for a native app; phases, GC, movers and the heap for a page) and the rig, with the command that reruns it.

Built like [mesh](https://github.com/Hona/mesh): one small native binary, a per-user daemon, CLI and MCP with the same names and JSON, and a built-in skill for opencode.
