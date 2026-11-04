---
name: strobe
description: Measure and explain performance of web pages, Electron apps, Bun or Node processes and any process. Use when the user asks whether a change is faster, why something is slow, janky or heavy, where memory goes, what the CPU or GPU is doing, or to write a benchmark.
---

# Strobe

strobe records a target on one timeline and answers with verdicts and handles (`rec_…`, `exp_…`) to drill into. It refuses measurements it cannot defend: a refused run is an answer.

## Reference

    strobe status [--calibrate]                    environment, validity preflight, recent handles
    strobe record <target> [--probes timing|attribution|memory] [--for 10s] [--headless] [--served <dir>]
    strobe record --bench <file.bench.ts>#<case> [--head <variant>]   a bench case (clicks, keys) under the attribution probes
    strobe record pid:<n>|proc:<name> [--probes timing|deep] [--for 10s]   any running process from outside: frames from
        the OS's present events, CPU, threads, IO, GPU, memory; deep: native stacks (kernel sampled profile, PDB symbols)
    strobe bench <file.bench.ts> --head <variant> [--base <variant> | --aa | --check] [--filter <case>] [--target <name>]
                 [--params k=v|all] [--primary <metric>] [--pairs auto|N] [--runs 5] [--power high|keep]
                 [--why auto|always|never] [--trace lean|full] [--probes memory --repeat 10] [--bisect good..bad]
                 [--pixels | --pixels-only] [--reserve-cpus auto|on|off]
        variant: a directory, a git ref, app:<id>, exe:<path>; every variant builds and serves with head's harness
        (the bench file's folder, strobe.json and its `harness` paths), so a base from before the bench works
    strobe query <rec_… | a..b | exp_…> <view> [args]    views: summary frames threads gpu hotspots alloc layout shifts gc heap leaks memory insights sql
        hotspots and alloc take --by function|file|package (hotspots: --native, --thread <name> for native samples);
        memory takes --by process|layer|allocator and --at <i>;
        a..b works with heap and memory; exp_… takes summary, progress and insights; --reveal shows heap strings
    strobe report experiment|recording <handle> [--format html|md|pr]    writes a file; show it in the Review pane; pr: tables for a PR body
    strobe update [<version>] [--from <path>] [--check]   the latest npm release (or a local build) over the installed copy

MCP: `status`, `record`, `bench`, `query`, `report`: same names, arguments and JSON as the CLI.

Targets: `https://…` · `cdp:<port>` · `app:<id>` (a profile in strobe.json `apps`, Electron or native) · `exe:<path>` (`--native` for a native app) · `cdp:ws://…` · `file:<path>` (import a trace, .cpuprofile or .heapsnapshot) · `pid:<n>` · `proc:<name>` (a running process, from outside)

Probe sets: `timing` (default) · `attribution` (CPU stacks, allocations, forced layouts, GC, a heap snapshot; its times are perturbed) · `memory` (three heap snapshots for leaks, no trace) · `deep` (native stacks of a process tree, Windows; perturbed)

On Windows, OS present events (what reached the screen, for any native or Chromium window) and native stacks need `strobe up`'s one UAC prompt (Performance Log Users and the deep-mode helper); without it, those metrics are refused and `status` says why. `record https://… --os-frames` puts them beside Chromium's own frames.

Exit codes: `0` · `1` error · `2` usage · `3` needs a human · `4` refused (invalid; the reason is in the result) · `5` inconclusive · `124` timeout · `255` unreachable

## A bench file

    import { bench, web, app } from "@hona/strobe"
    import { mockFeed } from "../e2e/mocks"
    export default bench({
      targets: {
        web: web({ viewport: { width: 1366, height: 768 },
          prepare: ({ playwright, url }) => mockFeed(playwright.page, { server: new URL(url).origin }),
          url: ({ prepared }) => prepared.path }),
        desktop: app("desktop", { home: "perf/fixtures/home" }),
      },
      params: { items: [24, 200] },
      async setup({ page }) { await page.waitFor("[data-feed]"); await page.idle() },
      cases: {
        scroll: { primary: "main.busy", run: async ({ page, phase }) => {
          await phase("up", () => page.scroll("[data-feed]", { by: -4000, speed: 3000 })) } },
        type: { primary: "interaction.p95", run: ({ page }) => page.type("[contenteditable]", "hello", { delay: 80 }),
          reset: async ({ page }) => { await page.press("ControlOrMeta+a"); await page.press("Backspace") } },
      },
    })

`page`: `goto` (relative paths resolve against the page) `click` `hover` `move(x, y)` `type` (one key event per character; `delay` ms between) `press` (`"ControlOrMeta+Shift+P"`) `down(key)`/`up(key)` (hold a key across calls: a game's `w`, a held `Shift`) `scroll` `wheel(sel, { dy })` `window("close")`, `act`, `query` (native) `waitFor` (`{ text, exact }`; `:focus` works) `idle` `wait` `snapshot(name)` (pixels for `--pixels`; nothing in timed runs); input calls take `{ until: "idle" | "frame" | selector | { selector, text, exact } }`. `reset` puts state back between runs and is never measured. Playwright only in `prepare` on `web` targets; what `prepare` returns is `ctx.prepared` in `setup`, `run` and `reset`, and it keeps working for the whole block. Mock APIs with `ctx.mock(pattern, handler)` in `prepare`: Playwright's `page.route` turns the HTTP cache off, so reloads refetch every script. `teardown` (target or file) stops what `prepare` started. Never call `page.eval` in `run`.

More in a bench file:
- `launch: { until, milestones?, profile?, before?, inspect? }` instead of `run`: one cold launch per block, timed from spawn to `until` (`launch.ready`); milestones `{ match: "text" | /re/, in: "output" | "page" | "<log glob under the home>" }` become `launch.<name>`; `launch.painted`/`launch.settled` from the window's pixels.
- `command("bun run dev:desktop", { cwd, env: { X: "{home}" }, unset: ["APP_*"] })`: an app started by a dev command, attached where it prints `DevTools listening on ws://…`; `before(ctx)` deletes caches for a cold start.
- `script("src/parse.ts")`: a Bun or Node module per block; cases `await ctx.call("parse", input)`, primary `cpu.work`.
- `native("my-app")` (a strobe.json profile with `kind: "native"`) or `native("C:/…/app.exe")`: a native app (no browser), isolated like an Electron app, measured from outside: `cpu.work`, `ui.cpu` (CPU of the main window's thread: the primary for "does it cost less"), `cpu.wakes` (thread wakes/s; `query <rec> threads` names the threads), `io.read`, `gpu.process.3d`, `gpu.vram`, `memory.app.private` and `memory.app.host` (what the app itself allocated: Windows charges the VRAM a GPU driver maps into the process as private commit), and from OS present events `frames.interval.p95`, `frames.dropped`, `interaction.p95` (input to the frame that showed it; counts inputs after which the app's accessibility tree changed). `page` drives any app with an accessibility tree (Windows UI Automation, posted window messages, never takes focus; macOS AX, Quartz events posted to the app, keys bring it to the front; Linux AT-SPI, XTest, keys focus its window): `page.click("nav-history", { until: "history-pane" })`; selectors are accessibility ids or key paths (`"slider/knob"`), `role=button`, `text=General`, `:focus`; `page.query("role=button")` lists what matches (find the keys with it); `page.act(sel, "invoke")` is an accessibility action (no input latency). Idle cases are `run: ({ page }) => page.wait(5000)`; launches `until: { window: "settled" }` or a selector. A directory or git variant runs strobe.json `build` and launches its `output` (`"CARGO_TARGET_DIR": "{cache}/cargo-target"` shares dependency builds across worktrees). Where the UI thread's time goes: `--why always`, then `query <rec> hotspots --thread <name> --phase <name>`. A debug build is refused for timing. A tray app is `native("my-app", { window: false })` with launches `until: { milestone: "up" }`. Where no driver reaches (no accessibility tree, a native Wayland window, a state only the app's CLI or files set), drive the copy through its own CLI or files: `ctx.home`, `ctx.env` and `ctx.exe` name its private home, environment and executable (`Bun.spawnSync([ctx.exe, "window", "show"], { env: ctx.env })`).
- A locked session: series that need no presenting window (script targets, leak runs, `window: false` native cases measuring `cpu.*`, `memory.*`, `gpu.vram`, `gpu.shared`, `io.*`, `count.*`, milestones; any native case with those under `--check`) run anyway and say `session locked`; compare those only with locked runs. Both blocks of a pair also share the display state (a locked display sleeps; `DisplayOff` says so). Everything with frames, pixels or a window waits for an unlock. Locked, a native app's accessibility tree, `page.act` and keys work; clicks are refused. GPU use (`gpu.process.3d`) is refused in locked blocks; GPU memory is kept. `record pid:…` on a locked session keeps CPU, threads, IO, GPU, memory and `--probes deep` stacks and refuses only frames; a profile with `"window": false` records without a window.
- Power: benches and timed recordings run on the fastest plan the device offers (High performance, or Windows 11's Best performance mode) and put the user's plan back after, even after a kill; `--power keep` keeps it, and on battery it is kept. The plan used is in the env line and the PR footer.
- Other load: during each timed block on Windows, strobe moves this user's other processes (agents, builds, editors) off the target's CPUs with a CPU set and puts them back after, even after a kill; processes started during the block follow within 200 ms. `--reserve-cpus off` opts out, `on` does it for `--check` too. Elevated processes stay (named in the `ReservedCpus` note, which goes in the PR footer).
- Several benches on one device take turns per block: one waiting for an unlock or building holds nothing, so locked-session series of another bench run meanwhile. `strobe status` lists every bench and what it waits for.
- Builds that work on any commit (`--base` before a build profile existed, `--bisect`): use the standard release profile with settings in `build.env` (Cargo: `--release` plus `CARGO_PROFILE_RELEASE_DEBUG=line-tables-only`, `CARGO_PROFILE_RELEASE_STRIP=false`, `CARGO_PROFILE_RELEASE_INCREMENTAL=false`), not a custom profile or feature.
- Large seed files (a model in a `home` seed, 64 MB and up) are shared read-only between launches, not copied per block; an app must not write them.
- `Throttled` means the speed check stayed slow on quiet CPUs; `Background` (refusing) names the process that kept the target's CPUs busy (an antivirus scanning new files, an update).
- Exact counters: `ctx.count(name)` in `run`, `globalThis.__strobeCount?.(name)` in the app → `count.<name>`; `net.requests{/api/*/message}` and `net.bytes`; `layout.shift` (then `query <rec> shifts` names what moved). A stable count is compared exactly.
- A WebGPU canvas's own frames, at its own frame rate: `canvas.frames`, `canvas.interval.p95` (a 60 fps game or a 30 fps animation on a faster display; presented frames count the display's vsyncs instead). Where the CPU goes by process: `cpu.renderer`, `cpu.gpu`, `cpu.browser`. Shader and pipeline work in the GPU process: `gpu.compile`.
- Source maps without changing what ships: strobe builds with `STROBE_SOURCEMAP=hidden`; let the bundler read it (Vite: `build: { sourcemap: process.env.STROBE_SOURCEMAP }`).
- `params: { files: { run: [100, 10_000], more: [1, 1_000] } }`: `more` only with `--params all`. `targets: ["web"]` on a case limits where it runs.
- `bun add -d @hona/strobe` pins strobe for the project: `bunx strobe …` runs that version, and editors and
  `tsc` get the bench types. strobe runs a bench file with its own SDK copy either way.

Primary metric: `main.work` for "does this cost less" (continuous; durations move in whole vsyncs; a native app's is `ui.cpu` or `cpu.work`, and `cpu.wakes` when idle), `main.busy` for scrolling and streaming, `interaction.p95` for typing and clicks, `frames.dropped.pct` when frames do drop, `launch.ready` for launches, `cpu.work` for scripts. Fewer than 6 pairs never gets a verdict. Validate a new file with `--check` first: each case once, pass or fail with a screenshot.

Everything about the app under test lives in its repo's `strobe.json`: `build` and `serve` for web targets, and `apps` profiles for Electron and native apps (`kind: "native"`, `exe` per OS, `env`, `unset`, `dirs`, `files`, `guard` for the user's own state, `protocols`, `extends`). Strobe itself knows no product. If an isolated launch disturbs the user's state, strobe stops: add what leaked to the profile.

## Recipes

Did my change help? `bench <file> --base origin/dev --head .` and read the verdict per series.
Why is it slow? `bench … --why always` (or `record --probes attribution`), then on the attribution recording: `query hotspots` (CPU by function, full stacks, original file:line:column through source maps), `query layout` (forced layouts by the call that forced them), `query alloc` (who allocates), `query gc`. `main.script`, `main.layout` and `main.style` in the bench result say where main-thread time went, with intervals.
What stands out? `query <rec or exp> insights`: forced layout, GC pressure, long tasks, work while idle, detached DOM, background load, unresolved time, and for an experiment the biggest base/head movers, each with the command that shows it.
Where is the memory? `query <rec> memory` (per process; `--by layer` or `--by allocator` for Chromium's own accounting). A native app or process: what holds it (host heap, GPU memory mapped in, unmapped commit, shared, files, images; `--by layer` for totals) with `gpu.vram` beside it. On Windows, `memory.app.private` also counts the VRAM a GPU driver maps into the process: quote `memory.app.host` for the app's own memory.
Which commit moved it? `bench <file> --filter <case> --bisect <good>..<bad>` (one series; each step is a full experiment).
Which objects are alive? `query <rec> heap` (classes by retained size, detached DOM, retainer paths); `query <base rec>..<head rec> heap` compares two.
Does a case leak? `bench <file> --filter <case> --head . --probes memory --repeat 10`: growth per run of the JS heap and DOM with an interval, and the objects from the first half still alive at the end (`query <rec> leaks`). On a native target it reads the processes' memory between runs: growth per run of `memory.app.host` with an interval (`query <rec> memory --at 0` for the first reading).
Is an app leaking over time? `record <target> --probes memory --for 60s` (do the suspect thing meanwhile, or point it at a page that does), then `query <rec> leaks`: objects from the first half still alive at the end, with what keeps them.
Is the method sound here? `bench <file> --aa --head .`.
Does it still look the same? Snapshot deterministic states with `page.snapshot(name)` (a still page, an app's own frame stepper), then `bench <file> --base … --head . --pixels-only`: each snapshot is `same` within the launch noise strobe measures (base run twice), or `differ` with a diff image.
Is a native app smooth, and where does its CPU go? `record proc:<name> --for 10s` while it animates (frames, drops, intervals from OS presents), then `record proc:<name> --probes deep` and `query <rec> hotspots --by package` (crates, std, OS modules, GPU driver; `--thread <name>` for one thread).
Where does the GPU process spend time (a frozen first frame, a slow canvas)? `query <rec> gpu` on an attribution recording, and `gpu.compile` as a bench metric. On a native or `pid:` recording, `query <rec> gpu` gives engine use and GPU memory per adapter from the OS.
What does a launch do before it is ready? `record --bench <file>#<launch case>`: its milestones, and `query <rec> threads` for each thread's wakes and CPU from the spawn until ready (no trace needed).
Does a native app wake up while idle? `record app:<id> --for 10s` (or `exe:<path> --native`), then `query <rec> threads` (wakes/s and CPU per named thread, the module each started in, and whose: app, driver such as `nvwgf2umx.dll`, or os) and `query <rec> insights`. `cpu.wakes` is context switches/s (waits and preemptions, driver threads included); the insight counts the app's own threads apart.
Writing the PR? `report experiment <exp> --format pr` gives the tables and the command that reruns them.

## Every performance claim in a PR comes from strobe

When a PR claims a performance change (faster, less CPU, fewer wakeups, less memory, smoother), its
body carries strobe's evidence, and nothing else stands in for it:

1. **Discovery**: the strobe command and recording id that pointed at the problem, and what it showed.
2. **The change**: what the code does differently.
3. **Results**: the output of `report experiment <exp> --format pr`, pasted verbatim. Don't retype
   numbers, round them, drop rows, drop the context line or warnings (locked session, power plan,
   cross-adapter, refused metrics), or rewrite verdicts. If a metric was refused or `unclear`, the PR
   says so.
4. **The bench**: the case's code, or a link to the bench file and case when it already exists. A new
   bench case ships in the same PR as the first change it measures.

Never put hand-measured timings, single recordings or `--check` smoke numbers in a results table:
label them as noisy discovery evidence, or leave them out.

## Notes

- Timing and attribution are separate blocks; never read timing from a `mixed` recording.
- `unclear` or `underpowered` means more pairs or a quieter machine, not "no change".
- Heap snapshots and traces can hold user content; views redact strings unless `--reveal`.
- Long benches keep running in the daemon: `query exp_… progress`.
- If `strobe` isn't on PATH, use the CLI path from the MCP instructions.
- Linux and macOS: frames on screen come from X Present (X11 on a real display, no compositing manager; Xvfb, compositors and Wayland are refused) and ScreenCaptureKit (needs Screen Recording); both give screen times only, so `frames.interval.*` and `frames.dropped*` are refused there while counts and `interaction.*` stand. Driving a native app needs, on macOS, the Accessibility permission for the app strobe runs under (`strobe status` names it: Terminal, the agent's app, or `~/.local/bin/mesh` remotely), and on Linux the session's accessibility bus (at-spi2-core) and an X11 display. Sandboxed Mac apps (TextEdit) are refused: their data lives in your `~/Library`. macOS counts wakes per process, IO as disk bytes, no VRAM per process, and samples stacks with `/usr/bin/sample` (untimed). Linux native stacks need `perf_event_paranoid` ≤ 2 or CAP_PERFMON; `status` says which. A macOS Cocoa app launches isolated only while its `~/Library` data does not exist (`LiveChannel` names the folder).
