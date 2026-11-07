---
name: strobe
description: Measure and explain performance of web pages, Electron apps, Bun or Node processes and any process. Use when the user asks whether a change is faster, why something is slow, janky or heavy, where memory goes, what the CPU or GPU is doing, or to write a benchmark.
---

# Strobe

strobe records a target on one timeline and answers with verdicts and handles (`rec_…`, `exp_…`) to drill into. It refuses measurements it cannot defend: a refused run is an answer.

## Reference

### `strobe status`

```text
strobe status [--calibrate]
```

- Environment, validity preflight, recent handles.
- `ios`: phones on a cable: model, iOS, USB/Wi-Fi, battery, temperature, locked; never the name.
- Read-only toward the phone: an untrusted one needs the user to unlock it and tap Trust.

### `strobe record`

```text
strobe record <target> [--probes timing|attribution|memory] [--for 10s] [--headless] [--emulate mobile|touch] [--served <dir>]
strobe record https://… --engine firefox [--probes timing|attribution] [--headless] [--for 10s]
strobe record --bench <file.bench.ts>#<case> [--head <variant>]
strobe record pid:<n>|proc:<name> [--probes timing|deep|waits] [--for 10s]
strobe record app:<id> --until-line "<regex>" [--stop-line "<regex>"]
strobe record <target> --video     (or record --bench … --video: a video of the check run; timing perturbed)
```

- `--for` (and every duration) needs its unit: `500ms`, `10s`, `2m`; a bare number exits 2.
- `--bench`: a bench case (clicks, keys) under the attribution probes.
- `pid:<n>|proc:<name>`: any running process from outside: frames from the OS's present events, CPU, threads, IO, GPU, memory.
  - `deep`: native stacks (kernel sampled profile, PDB symbols).
  - `waits`: `deep` plus off-CPU waits (`query <rec> waits`); costs the target at every context switch.
- `--until-line`: a native app from the line it prints when ready (to the next line), not a fixed settle.
- `--engine firefox`: the page in Firefox, from outside: CPU, GPU, memory, DOM nodes, and headed the OS's frames.
  - `--probes attribution`: Firefox's profiler and about:memory (`hotspots`, `layout`, `gc`, `memory`).
  - `--probes memory` refuses: Firefox gives no heap snapshots; use a bench leak run.

### `strobe bench`

```text
strobe bench <file.bench.ts> --head <variant>
    [--base <variant> | --aa | --check | --blocks N] [--filter <cases>] [--target <name>]
    [--params k=v|all] [--primary <metric>] [--pairs auto|N] [--runs 5] [--power high|keep]
    [--why auto|always|never] [--trace lean|full] [--probes memory --repeat 10] [--bisect good..bad]
    [--pixels | --pixels-only] [--reserve-cpus auto|on|off] [--noworse 2% | metric=2%,…]
    [--engine chromium|firefox] [--headless] [--video]
```

- variant: a directory, a git ref, `app:<id>`, `exe:<path>`; or a target of the file, `<target>[=<variant>]`, on both sides: an implementation comparison.
  - Every variant builds and serves with head's harness (the bench file's folder, strobe.json and its `harness` paths), so a base from before the bench works.
- `--filter`: case names, comma-separated, exact unless they hold `*` (`"window-closed*"` also runs `window-closed-memory`).
  - strobe prints what runs and what it skips before building.
- `--engine firefox`: page targets in Firefox (only the file's Firefox targets, when it has some).
  - No trace there: `main.*`, `layout.*`, `js.heap.*`, `span.*`, `frames.presented.p*` refuse.
  - Use `cpu.work` (every page block has it, in both engines), `phase.<name>`, `count.<name>`, `net.*`.
  - Headed: frames and `interaction.*` from the OS's presents (macOS, Linux: counts only).
  - Attribution blocks run Firefox's profiler: `hotspots` (source-mapped, with the browser's labels:
    `Reflow`, `Styles`, `Minor GC`, DOM calls), `layout` (forced reflows and style flushes by script),
    `gc`, `memory` (about:memory by process, layer, allocator), `insights`, `a..b`, `open`.
  - Windows samples every 2 ms through Firefox 160 (asked 1 ms: shorter makes every process's sampler spin a core), every 0.5 ms from 161.
  - `alloc`, `heap`, `leaks` and `style` refuse `EngineUnsupported` on a Firefox recording.
  - Leak runs (`--probes memory`) read about:memory after every run: page JS memory and DOM growth,
    no suspects (no heap snapshots).
  - Launch cases run (fresh profile each launch, `until` seen from the document's first script); no attribution
    block for them yet. WebKit refuses launch cases. A page's launch is never `profile: "warm"`, and never compared
    with an app's launch (`NotComparable`): it is a navigation in a running browser. `ctx.mock` and `ctx.addInitScript` work; `ctx.playwright` throws.
  - A case whose primary Firefox cannot measure is listed `refused` with the closest primary it has; the other cases still run.
- `--headless`: page targets headless, timed too; runs through a lock. Frame, latency, scroll, canvas and GPU-use metrics refuse `Headless`; `input.rate` stays. Pages only (an app or native target is a usage error).

### `strobe plan`

```text
strobe plan <file.bench.ts> [--base <v>] [--head <v>] [--filter …] [--effect 3%] [--pilot]
```

- Before a bench: pairs and minutes per series from earlier runs here (spread, block time), builds, other benches on the slot; runs nothing.

### `strobe run`

```text
strobe run [--pin auto] [--reserve-cpus on|off] -- <command>
strobe run --base "<command>" --head "<command>" [--aa | --check] [--primary cpu.work]
    [--metrics metric.<name>,…] [--pairs auto|N] [--runs 5] [--noworse 2%] [--require-verdict]
```

- `-- <command>`: your own harness as a bench block runs: the bench slot, the fastest plan, pinned, others kept off its CPUs.
- `--base … --head …`: a paired A/B of two commands, a full bench (verdict, `report`, PR block).
  - Each run starts the command and waits for its exit, measured from outside with what it starts.
  - `cpu.work` (primary), `run.duration`, `cli.launch` (the OS starting each command, on strobe's thread; not in `cpu.work`), `cpu.wakes` (not Windows), `memory.peak`, `io.read`, `io.write`.
  - A line `strobe:metric <name>=<value>` it prints adds to `metric.<name>` (lower is better; end the line with `higher` for a rate or score).
  - Declare one with `--metrics metric.parse_ms` to test it; every command of every run must print it, in one direction (`NoMetric`).
  - A simple command runs under `exec`; pipes, `;`, `&&`, redirects or `VAR=x cmd` run as a shell line.
  - Past the case's timeout, or leaving a process running (a daemon, a background job), fails the run (`RunIncomplete`).
  - `--aa --head "<command>"` checks the method; `--check --head` runs it once.

### `strobe query`

```text
strobe query <rec_… | a..b | exp_…> <view> [args]
```

- Views: `summary` `frames` `threads` `gpu` `hotspots` `waits` `alloc` `layout` `style` `shifts` `gc` `heap` `leaks` `memory` `insights` `actions` `sql`.
- `actions [--phase <name>] [--top N] [--reveal]`: what a bench block's hooks sent the target, in order (hook, run, phase, call, selector, outcome).
  - On a kept bench recording, or `exp_…` (every check case's, and the recordings a comparison kept); typed text, `act` values, `ctx.call` arguments and `ctx.exec` lines show as their length unless `--reveal`.
- `hotspots` and `alloc` take `--by function|file|package` (`hotspots`: `--native`, `--thread <name>` for native samples).
- `waits --thread <name> --phase <name> --by function|package`: where threads were off their CPUs and who woke them.
  - From a capture with `--probes waits` (Linux needs CAP_PERFMON; Windows the deep helper; macOS refuses `NoWaits` when it records).
- `frames` (pages): the renderer vs GPU process stage split and each worst frame's pipeline stages.
- `memory` takes `--by process|layer|allocator` and `--at <i>`.
- `a..b` (two recordings, per run each) works with `hotspots` `alloc` `layout` `style` `threads` `frames` `summary` `heap` `memory`.
- `exp_…` takes `summary`, `progress`, `insights`, `actions`, and those views for each series' base..head.
- `--reveal` shows heap strings, and in `actions` typed text, values, call arguments and exec lines.
- `threads --timeline`: each thread's CPU per second from the launch with the runs marked, exited threads too.
  - A driver thread that starts 25 s in and runs 30 s shows as such.

### `strobe open`

```text
strobe open <rec_… | exp_…[/head|base|<block>|<case>]> [--no-browser]
```

- The recording in the Perfetto UI (local, nothing leaves the machine):
  - its Chrome trace plus CPU profiles and native stacks as flame graphs (source-mapped / PDB names, file:line)
  - runs and phases, inputs, OS presents, threads' CPU and wakes, memory, validity
- `--no-browser` prints the URL.

### `strobe report`

```text
strobe report experiment|recording <handle> [--format html|md|pr]
strobe report experiment <exp> --format pr --update-pr <n> [-R owner/repo] [--force] [--no-card]
strobe report experiment <exp> --format card|badge [--out <file.png|file.svg>] [--theme light|dark|auto] [--badge-url] [--scoreboard]
```

- Writes a file; show it in the Review pane.
- `pr`: the experiment as one delimited block for a PR body, printed to stdout (the file's path on stderr).
- `--update-pr <n>`: replaces that experiment's block in PR n's body (or appends it) through gh.
  - The author's text stays; a hand-edited block is refused unless `--force`.
  - The result card leads the block (an SVG gh attaches, light or dark with the reader's theme; text only with an older gh or `--no-card`).
- `card|badge`: the result as an image; same bytes every run.
  - card 1200×630 (decided wins as large numbers with before → this PR and one dot per pair; otherwise what happened)
  - badge one line (`run time −84%`)
  - `--badge-url` prints a shields.io URL (sends the label to shields.io).
- Two implementations (sides are two targets): every report names each side by its target's `name`.
  - Headline: the case marked `hero: true`, else the biggest win that counts after Holm across journeys.
  - Card: that journey large on the left, every journey on a forest plot on the right, the scoreboard.
  - `--format badge --scoreboard`: `vs <base> | better in 4/5 · 1 gap`.
  - "Better in K of M" and "worse in K" count only after Holm across journeys; an unconfirmed
    regression is still listed in the caution. Gaps are never compared.

### `strobe track`

```text
strobe track <file.bench.ts> [--ref <rev> | --range a..b [--every N]] [--filter <case>]
    [--alert-command '<cmd>'] [--alert-webhook <url>]
```

- A point per series per commit in this device's history (6 single-build blocks, median and interval).
- Change points (E-divisive) are only suspicions: each is confirmed by a paired A/B of the commits around it, then bisected to the culprit.
- Only confirmed regressions alert.
- `--schedule commits|nightly|6h` with `--ref <branch>`: the daemon does it.
- A port (two implementations): `track <file> --base <target>=exe:<path> --head <target>=git:<branch>`. Each head commit gets 3 alternating pairs (`--pairs N`, from 3), one `ln(head/base)` point per journey with a 75% interval: evidence, not a verdict.
  - Pin the base (`=exe:`, `=app:`, `=git:<tag or commit>`, another repository's checkout): a branch, `HEAD` or a folder of the head's repository is refused.
  - A move is confirmed by an A/B of the head alone between the two commits, then bisected in the head's repository.
  - A comparator whose identity changed is an epoch ("Comparator updated (…)"), never an alert.
  - A journey going gap → compared (or back) is marked `parity`.

### `strobe history`

```text
strobe history [<ser_…|file>]
```

- Tracked series: points, sparkline, changes, culprits (`query history|changes`; `report history <ser_…>`).
- A port's file shows journeys × commits (`less -50%`, `same`, `gap`, `unclear -3%`), the latest scoreboard, changes and marks.
- `report history <file> --format card` draws the port's progress; `--format badge --scoreboard` draws `vs <base> (tracked, 3 pairs) | better in 4/5 · 1 gap` from the newest commit's points: grey, red only with `· 1 open regression` (a confirmed regression no confirmed improvement followed).

### `strobe look`

```text
strobe look <url|file> [--until <selector>] [--wait 1s] [--out shot.png] [--full | --element <selector>]
strobe look app:<id> | exe:<path> [--build] [--until <selector>]
```

- `<url|file>`: a page in a headless browser, nothing measured.
  - Title, the headings `expect` checks, visible text, console messages and errors, a screenshot.
  - `--element <selector>`: the screenshot of that element only (one section of a long report).
  - Check a UI loads instead of writing a browser script.
- `app:<id> | exe:<path>`: a native app launched isolated (`--build`: this checkout's build), then closed.
  - Window titles, the tree's dialogs and headings (what `expect` matches) and keyed elements.
  - Works on a locked session, so check `expect` with it.

### `strobe update`

```text
strobe update [<version>] [--from <path>] [--check]
```

- The latest npm release (or a local build) over the installed copy.

### MCP

- `status`, `record`, `bench`, `query`, `report`: same names, arguments and JSON as the CLI.
- `record` with `bench` needs no target; `bench` with `stop` or `resume` takes an experiment.
- An input a tool lacks, or of the wrong type, is refused; so is an unknown CLI flag (exit 2, naming the closest one).

### Exit codes

`0` · `1` error · `2` usage · `3` needs a human · `4` refused (invalid; the reason is in the result) · `5` inconclusive · `124` timeout · `255` unreachable

### result.json

- In result.json (`--json`), read each comparison's `verdict_kind` and a refused metric's `refusal.validator`, not the `verdict` words.
  - `verdict_kind`: `better`, `worse`, `same`, `no_worse`, `unclear`, `underpowered`, `refused`, `untested`, …
- Schema: `node_modules/@hona/strobe/schema/result.v1.json`.

## Targets & probes

### Targets

- `https://…`: Chromium, or Firefox with `--engine firefox` (stock Firefox only; `status` shows it)
- `cdp:<port>`
- `app:<id>`: a profile in strobe.json `apps`, Electron or native
- `exe:<path>`: `--native` for a native app
- `cdp:ws://…`
- `page.html` (or `file:page.html`): a local page in the browser
- `file:<path>`: import a trace, .cpuprofile or .heapsnapshot
- `pid:<n>` · `proc:<name>`: a running process, from outside

### Probe sets

- `timing` (default)
- `attribution`: CPU stacks, allocations, forced layouts, GC, a heap snapshot; its times are perturbed
- `memory`: three heap snapshots for leaks, no trace
- `deep`: native stacks of a process tree, Windows; perturbed
- `waits`: `deep` plus off-CPU waits with who woke each thread (`record pid:`, a native app's `record`, `bench --why always --probes waits`); heavier, at every context switch

### Windows: present events and native stacks

- On Windows, OS present events (what reached the screen, for any native or Chromium window) and native stacks need `strobe up`'s one UAC prompt (Performance Log Users and the deep-mode helper).
- Without it, present metrics are refused and `status` says why.
- Without it, native stacks in attribution blocks and `--probes deep` come from strobe's own sampler:
  - it suspends each busy thread once per 2 ms of its CPU, about 50 µs a sample; user frames only
  - the note and `query hotspots` say so
  - `record pid:… --probes stacks-unprivileged` asks for it with the helper too
- `record https://… --os-frames` puts them beside Chromium's own frames.

## A bench file

```ts
import { bench, web, app } from "@hona/strobe"
import { mockFeed } from "../e2e/mocks"

export default bench({
  targets: {
    web: web({
      viewport: { width: 1366, height: 768 },
      prepare: ({ mock, url }) => mockFeed(mock, { server: new URL(url).origin }),
      url: ({ prepared }) => prepared.path,
    }),
    desktop: app("desktop", { home: "perf/fixtures/home" }),
  },
  params: { items: [24, 200] },
  async setup({ page }) {
    await page.waitFor("[data-feed]")
    await page.idle()
  },
  cases: {
    scroll: {
      primary: "main.busy",
      run: async ({ page, phase }) => {
        await phase("up", () => page.scroll("[data-feed]", { by: -4000, speed: 3000 }))
      },
    },
    type: {
      primary: "interaction.p95",
      run: ({ page }) => page.type("[contenteditable]", "hello", { delay: 80 }),
      reset: async ({ page }) => {
        await page.press("ControlOrMeta+a")
        await page.press("Backspace")
      },
    },
  },
})
```

### `page`

- `goto`: relative paths resolve against the page; after a reset to about:blank, against the page's first address.
- `click`, `hover`, `move(x, y)`.
- `type`: one key event per character; `delay` ms between.
  - Paced: each key waits until the last one's frame is on the screen and the target rests (one 300 ms budget a key; OS presents where strobe follows them, else the page's own frames), so each is an `interaction.*` sample; a key sent before an earlier key's frame showed is none. Once the target never rests (an animation), the rest of the call goes `delay` apart.
  - `{ burst: true }`: keys `delay` apart, no waiting; `interaction.*` refuses `AmbiguousInteraction`. Measure burst typing by `cpu.work`, `main.work`, `ui.cpu`, `run.duration` or a phase.
- `press`: `"F9"`, `"PageUp"`, `"Control++"`, `"ControlOrMeta+Shift+P"`: one key table on every target.
- `click(sel, { button, modifiers, count })`.
  - Aims at the centre of the part of the element that shows (scrolled into view first if none does), in every engine and on native apps; a box taller than the window is clicked where it shows.
  - Nothing of it shows even in view (an `overflow: clip` box): refused `OutOfView`, naming the clipping ancestor.
  - Hit-tested: something over it at every point strobe tries (an overlay, a dialog's backdrop): refused `Covered`, naming it.
- `down(key)`/`up(key)`: hold a key across calls: a game's `w`, a held `Shift`.
  - `"Mouse"`, `"MouseRight"`, `"MouseMiddle"` hold a button, `{ at: sel | [x, y] }`, and `move` then drags.
- `drag(sel, path, { duration })`: press, move along keyframes `[[dx, dy], …]` or `t => [dx, dy]`, one move per display frame without waiting for each to be handled, release.
  - `input.rate` and `input.gap.p95` show how evenly the moves went out, beside the frames.
  - Use it for drags, not awaited `move`s.
- `scroll`, `wheel(sel, { dy })`.
- `window("close")`, `act`, `query` (native).
- `waitFor`: `{ text, exact }`; `:focus` works.
- `idle`, `wait`.
- `snapshot(name)`: pixels for `--pixels`; nothing in timed runs.
- Input calls take `{ until: "idle" | "frame" | selector }`.

### Selectors

- Selectors are one grammar on pages and native apps:
  - a base (CSS on a page, an accessibility key or key path on a native app)
  - then `role=<role>`, `text=<text>` (`text=="General"`: all of it; quotes hold spaces), `index=<n>`, `:focus`
  - `role=heading text="General"`, `.row text=Draft index=1`
  - or `{ selector, role, text, exact, index, focus }`

### Case lifecycle

- `reset` puts state back between runs and is never measured.
- Waits inside a run that are not the journey: `await ctx.untimed(() => page.idle())` between two clicks.
  - Its time, the target's CPU in it (`cpu.*`, `ui.*`, `io.*`, `main.*`), its frames, counts and inputs sent in it leave the run's numbers.
  - An input just before it still counts: its frame may come during the wait.
  - For waits only: work started in it (a page load, a big setup) can leave a cost the run pays after it.
  - `gpu.process.3d`, `energy.*`, `cpu.instructions`, `cpu.idle_wakes`, `net.*`, `canvas.*`, `js.heap.retained` are refused (`Untimed`) in a block that used it.
  - `--check` and the bench text print an `untimed` line with what it left out.
- Playwright only in `prepare` on `web` targets, and Chromium only: prefer `ctx.addInitScript` and `ctx.mock`, which work in every engine.
- What `prepare` returns is `ctx.prepared` in `setup`, `run` and `reset`, and it keeps working for the whole block.
- Scripts before the page's own (a frame counter, seeded `localStorage`): `ctx.addInitScript(fn, arg?)` in `prepare` or `setup`, Playwright's `page.addInitScript` in any engine; new documents only.
- Mock APIs with `ctx.mock(pattern, handler)` in `prepare`: Playwright's `page.route` turns the HTTP cache off, so reloads refetch every script.
- `teardown` (target or file) stops what `prepare` started.
- Random workloads (a fuzzer, a random walk): draw from `ctx.random()`, never `Math.random()`.
  - Every hook call starts its stream over: each run sends the same inputs, so warmup converges and base and head run the same workload.
  - `ctx.seed` is the case's seed; to seed the app too, pass it in yourself: `ctx.addInitScript((s) => …, ctx.seed)`. strobe never patches the app's `Math.random`.
  - strobe draws the experiment's seed and prints it (`seed 42`); `bench … --seed 42` replays it, and `query <rec> actions` shows what was sent.
  - A failed `--check` prints the seed; every check case keeps its actions (`query <exp> actions`).
  - `--bisect` runs every step with one seed; `track` uses one per repository and bench file (`--seed` to pick it).
- Never call `page.eval` in `run`.

### More in a bench file

- `launch: { until, milestones?, profile?, before?, inspect? }` instead of `run`: one cold launch per block, timed from spawn to `until` (`launch.ready`).
  - milestones `{ match: "text" | /re/, in: "output" | "page" | "<log glob under the home>" }` become `launch.<name>`
  - `launch.painted`/`launch.settled` from the window's pixels
- `command("bun run dev:desktop", { cwd, env: { X: "{home}" }, unset: ["APP_*"] })`: an app started by a dev command, attached where it prints `DevTools listening on ws://…`.
  - `before(ctx)` deletes caches for a cold start.
- `script("src/parse.ts")`: a Bun or Node module per block; cases `await ctx.call("parse", input)`, primary `cpu.work`.
- `cli("./target/release/tool input.txt")`: a command run to its exit per `ctx.exec()` (`run: (ctx) => ctx.exec()`) in the variant's folder, measured from outside.
  - `cpu.work`, `run.duration`, `cli.launch`, `cpu.wakes`, `memory.peak`, `io.*`, `metric.<name>` from its `strobe:metric` lines; a nonzero exit fails the run.
  - No attribution blocks (the command ends inside each run).
- `params: { files: { run: [100, 10_000], more: [1, 1_000] } }`: `more` only with `--params all`.
- `targets: ["web"]` on a case limits where it runs.
- Source maps without changing what ships: strobe builds with `STROBE_SOURCEMAP=hidden`; let the bundler read it (Vite: `build: { sourcemap: process.env.STROBE_SOURCEMAP }`).
- `bun add -d @hona/strobe` pins strobe for the project: `bunx strobe …` runs that version, and editors and `tsc` get the bench types.
  - strobe runs a bench file with its own SDK copy either way.

### Native apps

- `native("my-app")` (a strobe.json profile with `kind: "native"`) or `native("C:/…/app.exe")`: a native app (no browser), isolated like an Electron app, measured from outside:
  - `cpu.work`
  - `ui.cpu` (CPU of the main window's thread: the primary for "does it cost less")
  - `cpu.wakes` (thread wakes/s; `query <rec> threads` names the threads)
  - `io.read`, `gpu.process.3d`, `gpu.vram`
  - `memory.app.private` and `memory.app.host` (what the app itself allocated: Windows charges the VRAM a GPU driver maps into the process as private commit)
  - and from OS present events `frames.interval.p95`, `frames.dropped`, `interaction.p95` (input to the frame that showed it; counts inputs after which the app's accessibility tree changed)
  - A click whose control acts on the press (tabs, menus, sidebars) is timed from its press, one sample either way; `interaction.n.press` says how many, and `query <rec> frames` labels them `press`.
  - Focus moving or a pressed look on the press never makes it a press sample, nor does a control that changes again after the release.
  - A click that gives no sample in `query <rec> frames` ("changed no tree") on an app that announces late: let it go idle between clicks with `ctx.untimed(() => page.idle())`.
- `page` drives any app with an accessibility tree: `page.click("nav-history", { until: "history-pane" })`.
  - Windows: UI Automation, posted window messages, never takes focus.
  - macOS: AX, Quartz events (keys to the app, which they bring to the front; clicks and wheel at the point, as a person's).
  - Linux: AT-SPI, XTest, keys focus its window; X11 or XWayland only: a native Wayland window is invisible, so `unset: ["WAYLAND_DISPLAY"]` in the app profile.
- Selectors are accessibility ids or key paths (`"slider/knob"`) with the filters above (`role=button text="Save"`).
- `page.query("role=button")` lists what matches with key, name, value, `checked` and `enabled` (find the keys with it; assert a toggle's state with `checked`).
- `page.act(sel, "invoke")` is an accessibility action (no input latency); `act(sel, "setValue", { value })` fails when the app does not take the value.
- Idle cases are `run: ({ page }) => page.wait(5000)`; launches `until: { window: "settled" }` or a selector.
- Native targets wait for a line the app writes (`{ line, at }`, `at` on strobe's clock):
  ```ts
  const saved = ctx.waitForLine(/saved/, { in: "output" | "<log glob under the home>" })
  await page.click("save")
  await saved
  ```
  - `"output"` needs `native(…, { output: true })` and an app that flushes each line.
- A case's `gpuCache: "cold"` overrides its target's.
- A directory or git variant runs strobe.json `build` and launches its `output` (`"CARGO_TARGET_DIR": "{cache}/cargo-target"` shares dependency builds across worktrees).
- Where the UI thread's time goes: `--why always`, then `query <rec> hotspots --thread <name> --phase <name>`.
- A debug build is refused for timing.
- A tray app is `native("my-app", { window: false })` with launches `until: { milestone: "up" }`.
- Where no driver reaches (no accessibility tree, a native Wayland window, a state only the app's CLI or files set), drive the copy through its own CLI or files:
  - `ctx.home`, `ctx.env` and `ctx.exe` name its private home, environment and executable (`Bun.spawnSync([ctx.exe, "window", "show"], { env: ctx.env })`).

### Webview apps

- An app that embeds the OS's webview (WebView2 on Windows) is `webview("my-app")` (a profile with `kind: "webview"`) or `webview("C:/…/app.exe")`.
  - Launched isolated like an Electron app; strobe opens the webview's debugging port through its environment, with no change to the app.
  - `page` and every page metric work as on `app()` targets; CPU and memory cover the app and its webview processes.
  - Linux (WebKitGTK) and macOS (WKWebView): refused `EngineUnsupported` before anything launches; measure the app with `native("my-app")` through its accessibility tree.

### Locked sessions

- A locked session: series that need no presenting window run anyway and say `session locked`; compare those only with locked runs. They are:
  - script targets, leak runs, headless pages (`--headless`)
  - `window: false` native cases measuring `cpu.*`, `memory.*`, `gpu.vram`, `gpu.shared`, `io.*`, `count.*`, `phase.*`, `energy.*`, milestones
  - any native case with those under `--check`
- Both blocks of a pair also share the display state (a locked display sleeps; `DisplayOff` says so).
- Everything with frames, pixels or a window waits for an unlock.
- Locked, a native app's accessibility tree, `page.act` and keys work; clicks are refused.
- GPU use (`gpu.process.3d`) is refused in locked blocks; GPU memory is kept.
- `record pid:…` on a locked session keeps CPU, threads, IO, GPU, memory and `--probes deep` stacks and refuses only frames.
- A profile with `"window": false` records without a window.

### Power and load

- Power: benches and timed recordings run on the fastest plan the device offers (High performance, or Windows 11's Best performance mode).
  - They put the user's plan back after, even after a kill.
  - `--power keep` keeps it, and on battery it is kept.
  - The plan used is in the env line and the PR footer.
- Other load: during each timed block on Windows and Linux, strobe moves this user's other processes (agents, builds, editors) off the target's CPUs and puts them back after, even after a kill.
  - Windows: a CPU set; Linux: each thread's affinity.
  - Processes started during the block follow within 200 ms.
  - `--reserve-cpus off` opts out, `on` does it for `--check` too.
  - `--pin auto` (the default) picks the target's CPUs: the V-Cache CCD; equal CCDs: the first; a hybrid CPU: its fastest cores; otherwise all but the last 1-2 whole cores, which strobe and the runner get. Fewer than 4 cores, more than 64 CPUs, or macOS: unpinned, and the plan line names the `--pin` list that would pin anyway.
  - A virtual machine with one L3 and 4 or more cores now pins and reserves under `--pin auto`: no explicit CPU list needed.
  - Elevated processes stay (named in the `ReservedCpus` note, which goes in the PR footer).
  - macOS has no affinity.
- Several benches on one device take turns per block: one waiting for an unlock or building holds nothing, so locked-session series of another bench run meanwhile.
  - `strobe status` lists every bench and what it waits for.
- `Throttled` means the speed check stayed slow on quiet CPUs.
- `Background` (refusing) names the process that kept the target's CPUs busy (an antivirus scanning new files, an update).

### Builds and launches

- What a build needs on one OS (a toolchain, a compiler) goes in strobe.json, not the user's shell.
  - `build.windows`, `build.macos` or `build.linux` overlay `build` there (`env` merged, other keys replaced).
  - e.g. `"windows": { "env": { "RUSTUP_TOOLCHAIN": "stable-x86_64-pc-windows-msvc" } }`
  - A failed build shows its last lines and saves the whole log.
- Builds that work on any commit (`--base` before a build profile existed, `--bisect`): use the standard release profile with settings in `build.env`, not a custom profile or feature.
  - Cargo: `--release` plus `CARGO_PROFILE_RELEASE_DEBUG=line-tables-only`, `CARGO_PROFILE_RELEASE_STRIP=false`, `CARGO_PROFILE_RELEASE_INCREMENTAL=false`.
- GPU shader caches: a fresh home would make the GPU driver compile every shader again in every block (NVIDIA once burned 5 s of a 6 s run on a driver thread).
  - `gpuCache: "warm"` (the default for `native()`, `webview()`, `app()` and `exe()` targets, or `"gpuCache"` in the app profile) gives each variant its own cache store.
  - That store is filled by discarded warm-up blocks before the first pair, never shared with the other variant or yours.
  - `"cold"` measures a first launch after an install.
  - `record` and `record --bench` share one store per executable: the first recording of a build is cold, later ones warm (`--gpu-cache cold`).
  - macOS keeps Metal's cache per app outside the home: always the user's, warm.
- Large seed files (a model in a `home` seed, 64 MB and up) are shared read-only between launches, not copied per block; an app must not write them.
- What strobe launches is the target's whole process set: its tree plus everything started for its home (an app's detached service, a double-forked daemon).
  - It is counted in `cpu.*`, wakes and memory and ended with the target, also when a block fails early.
  - On Windows each launch is a job, so a strobe that dies takes its launches with it.
  - On macOS it holds the XPC services the app is responsible for (a web view's web content, GPU and network processes), also for `record pid:` of an app opened from Finder.
  - An app bundle's executable launches as its own responsible app, as from Finder: macOS asks the bundle's own permissions, not those of the app strobe runs under.
  - A bare executable's XPC services are responsible to your terminal or daemon and are not counted: measure a web view app as its `.app` bundle.
  - `! Unattributed` means a bundle's launch did not become its own responsible app: its numbers hold its own processes only.
- A strobe that crashed or was killed may leave its isolated app or browser running (on Windows only what broke away from its job).
  - The next `bench`, `record` or `look` ends it by itself (it prints `ended <exe> (pid N): the strobe that launched it (pid M) is gone and left it running`).
  - Everything strobe launches carries `STROBE_HOME` and `STROBE_OWNER` in its environment, so it never touches a copy you started or a command that merely names a strobe folder.
  - `LiveChannel` then names what still runs: another strobe still measuring that build (wait for it), or your own copy with the command that closes it.

### What was on screen

- Every windowed block records its window titles, dialogs and headings (a page: title, address, headings) and a perceptual hash with a thumbnail, before and after its runs.
- Output says `shown  'App Settings · General'` per series, the PR block an `On screen:` line, and `ScreenChanged` warns when base and head (or some blocks) showed different screens.
- Read it before trusting a number: a first-run dialog over the pane once turned a whole A/B into a measurement of the dialog.
- Pin it with `expect: { title: /Settings/, heading: "General", visible: "nav-general" }` on the case or target: a block on another screen is refused `WrongScreen` instead of timed.
- To watch it: `--video` on `record`, `bench --check` or a comparison (one untimed filmed run per side after the timed pairs; never in them).
  - `strobe report experiment <exp> --open`: both sides' players on one clock, a strip of the runs and each input strobe sent; click one to seek there.
  - `screen` (the OS's copy of the window: headed apps and pages) or `renderer` (a Chromium page's own frames: headless, locked sessions). Two kinds never compare side by side.
  - Needs ffmpeg for `renderer` and Linux X11 (strobe says how to install it); headless Firefox/WebKit, `window: false`, scripts and cli targets get none.

### Every run starts from the same state

- Before each run and after the last reset, strobe compares the values of editable controls and the states of check boxes, radio buttons and switches with run 1's start (not labels, clocks, headings or focus).
- A reset that takes back nothing, or a typing run with no reset, is refused `ResetIncomplete`, naming the control: `the reset did not restore search "cache keycache key" (was "")`.
  - Controls "not there at run 1" mean the screen was still loading then (wait in setup) or the run left something open.
  - strobe waits for the first start to settle; a control that changes by itself (a clock, a meter) is not compared.
  - Select-all is `ControlOrMeta+a`: Control+a moves to the line start on macOS.
- `resetCheck: false` on a case whose runs build on each other on purpose.

### Refused, never "no value"

- A declared metric gets a value or a typed refusal:
  - `NotOnThisOs`: the OS cannot read it (macOS: `gpu.vram`, `gpu.shared`, `memory.app.host`, `ui.wakes`; a GPU whose per-process statistics did not open or left a process unread; Windows where cycle times are not time, as on Arm before 11 24H2 (core clock cycles) or CPUs that disagree: every metric made of cycles, `cpu.work`, `cpu.app.total`, `cpu.process.total`, `ui.cpu`, `cpu.renderer`, `cpu.gpu`, `cpu.browser`, `cpu.foreign`, `cpu.foreign.max`; `strobe status` prints the cycle clock on its `cycles` line, checked once per boot and cached).
  - `NotForTarget`: the target or case has none (a run's metric on a launch case, `ui.*` without a window).
  - `NoMetric`: a declared `metric.<name>` a run's command did not print; `NoWaits`: `query waits` on a capture without waits, saying why.
- A native app that exits is refused `RunIncomplete` with its exit code and, with `output: true` on the target, its last output lines.
- `strobe <command> --help` lists that command's flags.
- `strobe config` checks the project's strobe.json and every app profile on this OS without a bench, read-only.
- `strobe record page.html` records a local page.

### Phases, spans and counters

- A span inside a run: `await phase("finish", () => fetch(…))` gives `phase.finish`, its wall time per run from strobe's own marks (declare it as the primary or a metric).
  - A native app's CLI or local API call inside a phase times a stage end to end.
- The app's own spans: `span.<name>` sums a page's (or Electron app's) `performance.measure("<name>")` calls per run, timed by the browser; `NoSpan` when no run measured it.
- Energy and instructions on macOS, free:
  - `energy.cpu` (mJ per run)
  - `cpu.instructions` (per run; far tighter run to run than `cpu.work`, so small CPU changes decide in fewer pairs)
  - `cpu.idle_wakes` (/s: what Activity Monitor's Energy Impact weighs)
  - Linux has `cpu.instructions` only, from perf events that slow the target's context switches, so strobe counts them in counter blocks of their own and keeps the timing blocks clean.
  - `i Counters` says how many pairs each kind got.
  - Windows refuses all three. Never estimated: `NoCounter` says why.
- Exact counters:
  - `ctx.count(name)` in `run`, `globalThis.__strobeCount?.(name)` in the app → `count.<name>`
  - `net.requests{/api/*/message}` and `net.bytes`
  - `layout.shift` (then `query <rec> shifts` names what moved)
  - A stable count is compared exactly.
- A WebGPU canvas's own frames, at its own frame rate: `canvas.frames`, `canvas.interval.p95`.
  - A 60 fps game or a 30 fps animation on a faster display; presented frames count the display's vsyncs instead.
- Where the CPU goes by process: `cpu.renderer`, `cpu.gpu`, `cpu.browser`.
- Where a page's frame time goes: `frames.renderer.p95` (renderer stages: main and compositor threads), `frames.viz.p95` (GPU process stages: raster and the display compositor's draw and swap).
  - Pipeline stage spans (queueing included), not busy time, so neither says CPU- or GPU-bound; the waits for the display are in neither.
  - Refused (`NoPresentSource`) when under 95% of the presented frames carry stages.
- Shader and pipeline work in the GPU process: `gpu.compile`.

### Primary metric

- `main.work` for "does this cost less" (continuous; durations move in whole vsyncs; a native app's is `ui.cpu` or `cpu.work`, and `cpu.wakes` when idle).
- `main.busy` for scrolling and streaming.
- `interaction.p95` for typing and clicks.
- `frames.dropped.pct` when frames do drop.
- `launch.ready` for launches.
- `cpu.work` for scripts.

Fewer than 6 pairs never gets a verdict.

- Validate a new file with `--check` first: each case once, pass or fail with a screenshot. A case with a reset runs once more after it (unmeasured), so a run that cannot start from what the reset leaves fails the check.
- `NotSteady` says whether the work grew (add a reset, or fix what the reset leaves behind) or only swung from run to run (pin CPUs, quiet the machine, more work per run).
- A `url()` target loads its address whatever the build: `--base <ref> --head .` on one is refused `VariantMismatch` before anything builds (compare `--base url:<a> --head url:<b>`).
- `--max-time` gives each series an equal share of the time left when it starts (checked from its first look), so the first cannot spend the rest's.
- No display (locked, a server)? `--headless` runs page cases (check or timed) with frame and latency metrics refused.
  - Headed on Linux without `DISPLAY`/`WAYLAND_DISPLAY` refuses at once (`NoPresentSource`). `strobe up` calibrates headless there (frame checks skipped).
  - On a software GPU (Xvfb, a VM): CPU metrics stay, frame and GPU metrics are refused (`SoftwareGpu`); a primary among them refuses the series.
  - Headless, what frames cost the page is `main.paint` (prepaint and paint on the main thread, ms per run) beside `main.style` and `main.layout`. There is no headless frame-time metric: a headless compositor's clock is not a display's.
- Emulate a device on a page target: `web({ emulate: "mobile" })` (a 390×844 phone at scale 3, headless only, Android user agent, touch) or `"touch"` (a desktop with a touch screen), as `record --emulate`. Checked before `setup` (`DeviceMismatch`); WebKit refuses it; the series' window line says `390×844 @3x · emulated mobile`. Load and launch cases work on a phone; clicks, taps, hovers, drags and scrolls there are refused `EngineUnsupported` (strobe sends no touch input yet), and so are `type`/`press` with an element to click into.
- Page targets are desktops, headless too: strobe gives the page a fine pointer that hovers and turns touch off (`(hover: hover)`, `(pointer: fine)`, no touch points, no `ontouchstart`, touch screens included), so a site never takes its touch paths in a desktop bench. Checked before `setup`; a page that sees otherwise is refused `DeviceMismatch` (WPE headless on Linux: use `xvfb-run -a` with WebKitGTK, or Chromium/Firefox). No `STROBE_BROWSER` wrapper with hover/pointer `--blink-settings` is needed (strobe's replaces a wrapper's).
  - App pages (Electron, webviews) keep the machine's input; the result's `input` says what they saw (`desktop`, `hybrid`, `touch`, `none`). A page against an app whose pages saw different input is refused `VariantMismatch`.
- Chromium: `STROBE_BROWSER`, else Chrome/Chromium/Edge, else Playwright's (`PLAYWRIGHT_BROWSERS_PATH` or its cache; Chrome for Testing included).
- Comparing engines? `cpu.work` is the one primary both have; a Firefox page's main-thread split does not exist.
- WebKit pages: `--engine webkit` (macOS: Safari Technology Preview, else Safari, headed only; Linux: WebKitGTK headed; `--headless` runs WebKitGTK on the session's display, so use `xvfb-run` on a server (WPE's headless pages see a touch device: refused); never on Windows).
  - Exit 3 "remote automation is off": a person runs `safaridriver --enable` once (admin password).
  - `LiveChannel`: that Safari is running; quit it or install Safari Technology Preview.
  - Refused there (`EngineUnsupported`): `main.*`, `net.*`, `input.rate`, `ctx.mock`, clicks in WPE headless (it drops pointer input; keys and scrolling work), and `count.*`, `canvas.*`, `interaction.*` of a run that loads a page (no preload in WebDriver Classic: keep runs on one page).
- `--blocks N` times one build alone (what `track` records).

### `strobe.json`

- Everything about the app under test lives in its repo's `strobe.json`:
  - `build` and `serve` for web targets
  - `apps` profiles for Electron, webview and native apps (`kind: "webview" | "native"`, `exe` per OS, `env`, `unset`, `dirs`, `files`, `guard` for the user's own state, `protocols`, `extends`)
- Strobe itself knows no product.
- If an isolated launch disturbs the user's state, strobe stops: add what leaked to the profile.
- The nearest strobe.json upward from the bench file wins (a package's own in a monorepo, else the repository's).
  - `build.cwd` and `serve.cwd` resolve against the variant's checkout root (the repository root, as a git ref's worktree is), never the folder you named: `--head .` from a subfolder builds the same as from the root.
- strobe.json takes `//` comments and trailing commas.
- A key strobe does not read or a value of the wrong type fails (exit 2) with its line and the key it meant.
  - A problem in a profile of your own `<state>/strobe.json` fails only the launch that uses it.
- `"$schema": "./node_modules/@hona/strobe/schema/strobe.v1.json"` gives an editor the keys.

## Recipes

- **Did my change help?** `bench <file> --base origin/dev --head .` and read the verdict per series.
- **Did my refactor or dependency bump make it worse?** `bench <file> --base origin/dev --head . --noworse 2%` (or `noworse: "2%"` on the case; `{ "cpu.wakes": "5%" }` per metric).
  - `no worse (≤2%)`, `worse (>2%)` or `unclear`; it decides in fewer pairs than `same`.
- **How long will it take?** `plan <file> --base … --head .` first: pairs and minutes per series.
  - `underpowered` there means choose a primary with more work per run.
  - A running bench prints an ETA after every pair.
- **Rerunning?** Every experiment counts the others on the same two variants.
  - The text, the result's `ledger` and the PR table say "3rd experiment on these variants; earlier: …" with what changed.
  - Rerunning until one says "faster" shows; paste the ledger line as it is.
  - Another `--seed` counts as a change only when the hooks used `ctx.random()` or `ctx.seed`.
- **Why is it slow?** `bench … --why always` (or `record --probes attribution`), then on the attribution recording:
  - `query hotspots` (CPU by function, full stacks, original file:line:column through source maps)
    - Its header (and each attribution block's line, `query <rec> summary`, result `attribution_sampling`) gives the interval the sampler kept, not the one asked: `sampled every 2.0 ms (asked 1 ms: 2.0× coarser)`.
    - Windows keeps about 0.5 ms at best (Chromium asks 0.4 ms there); Firefox 160 and older on Windows 2 ms.
    - Times hold at any interval (each sample weighs its own gap); a function shorter than one gap shows only by chance.
    - `(strobe driver)` and `(native, via strobe driver)` are strobe's own waits and the layouts they forced, never the app's; over 2% of an attribution block's samples (or a timed block's reads over 2% of a run or 8 a run) warns `ObserverCost`.
    - A load case's profile covers each measured run whole, its navigation included, never the reset.
  - `query layout` (forced layouts by the call that forced them)
  - `query style` (what invalidated style: the class, attribute, pseudo-class or custom property that changed, the nodes, the selector, the script line)
    - attribution blocks trace it for a series declaring `main.style` or `style.count`, or with `--style`
  - `query summary` (per run: style ms with elements restyled, layout, paint)
  - `query alloc` (who allocates)
  - `query gc`
  - What changed between base and head: `query <exp> hotspots` (or `alloc`, `layout`, `style`, `threads`; `<base rec>..<head rec>` for any two).
    - Rows by the size of their change per run, with the stacks it concentrated in.
    - `~` marks a change inside its sampling interval (one launch per side: not a verdict).
    - The HTML report's diff flame shows the same as a call tree.
  - `main.script`, `main.layout`, `main.style` and `main.paint` in the bench result say where main-thread time went, with intervals.
- **What stands out?** `query <rec or exp> insights`: forced layout, GC pressure, long tasks, work while idle, detached DOM, background load, unresolved time.
  - For an experiment, the biggest base/head movers, each with the command that shows it.
- **Where is the memory?** `query <rec> memory` (per process; `--by layer` or `--by allocator` for Chromium's own accounting).
  - A native app or process: what holds it (host heap, GPU memory mapped in, unmapped commit, shared, files, images; `--by layer` for totals) with `gpu.vram` beside it.
  - On Windows, `memory.app.private` also counts the VRAM a GPU driver maps into the process: quote `memory.app.host` for the app's own memory.
- **Is this command faster than that one?** `run --base "<old command>" --head "<new command>"` (a build flag, a binary, an input).
  - Print `strobe:metric <name>=<value>` from the program for its own stages; `--metrics metric.<name>` tests one.
- **What is a thread waiting on (a lock, IO, a timer), and who wakes it?** `record pid:<n> --probes waits --for 10s` (or `record app:<id> --probes waits`), then `query <rec> waits --thread <name>`.
  - Rows by where it waited (its first frame outside the OS and runtime), with the waker's thread and frame, or an interrupt.
- **Is a slow page frame in the renderer or the GPU process?** `frames.renderer.p95` vs `frames.viz.p95`, and `query <rec> frames` (each worst frame's stages).
- **Which commit moved it?** `bench <file> --filter <case> --bisect <good>..<bad>` (one series; each step is a full experiment).
- **Did anything regress since last week?** `track <file> --range <old>..<branch> --every 3` (or a `--schedule` on the branch), then `history <file>` or `query changes`.
  - A `confirmed` change has a paired verdict and a culprit; `suspected`, `unclear` and `rejected` are not findings.
  - A series is one device and environment: a new strobe, driver, OS build, power plan or lock state starts another.
- **Which objects are alive?** `query <rec> heap` (classes by retained size, detached DOM, retainer paths); `query <base rec>..<head rec> heap` compares two.
- **Does a case leak?** `bench <file> --filter <case> --head . --probes memory --repeat 10`.
  - Growth per run of the JS heap and DOM with an interval, and the objects from the first half still alive at the end (`query <rec> leaks`).
  - On a native target it reads the processes' memory between runs: growth per run of `memory.app.host` with an interval (`query <rec> memory --at 0` for the first reading).
- **Is an app leaking over time?** `record <target> --probes memory --for 60s` (do the suspect thing meanwhile, or point it at a page that does), then `query <rec> leaks`.
  - Objects from the first half still alive at the end, with what keeps them.
- **Is the method sound here?** `bench <file> --aa --head .`.
- **Does it still look the same?** Snapshot deterministic states with `page.snapshot(name)` (a still page, an app's own frame stepper), then `bench <file> --base … --head . --pixels-only`.
  - Each snapshot is `same` within the launch noise strobe measures (base run twice), or `differ` with a diff image.
- **Is a native app smooth, and where does its CPU go?** `record proc:<name> --for 10s` while it animates (frames, drops, intervals from OS presents).
  - Then `record proc:<name> --probes deep` and `query <rec> hotspots --by package` (crates, std, OS modules, GPU driver; `--thread <name>` for one thread).
- **Where does the GPU process spend time (a frozen first frame, a slow canvas)?** `query <rec> gpu` on an attribution recording, and `gpu.compile` as a bench metric.
  - On a native or `pid:` recording, `query <rec> gpu` gives engine use and GPU memory per adapter from the OS.
- **What does a launch do before it is ready?** `record --bench <file>#<launch case>`: its milestones, and `query <rec> threads` for each thread's wakes and CPU from the spawn until ready (no trace needed).
- **Does a native app wake up while idle, or drain the battery at rest?** `record app:<id> --for 10s` (or `exe:<path> --native`), then `query <rec> threads` and `query <rec> insights`.
  - `threads`: wakes/s and CPU per named thread, the module each started in, and whose: app, driver such as `nvwgf2umx.dll` or `radeonsi_dri.so`, or os.
  - `cpu.wakes` is context switches/s (waits and preemptions, driver threads included); the insight counts the app's own threads apart.
  - On macOS the `counters` line gives `energy.cpu`, `cpu.instructions` and `cpu.idle_wakes`; bench them with an idle case.
- **When did it happen, and what ran beside it?** `strobe open <rec>` (or `<exp>/head`).
  - Perfetto shows a long task beside the presents it delayed, a driver thread compiling while the UI waits, and lets a person browse stacks (drag over a Callstacks track for the flame graph).
  - Numbers and verdicts still come from strobe's views (`hotspots`, `threads`, `frames`); Perfetto is for seeing and browsing.
  - The track "strobe itself (its own process)" is strobe's own CPU, GPU and memory over the recording: check it before blaming the app for a busy moment.
- **A native click's `until` never showed and the error says the element moved?** It moved or resized as strobe clicked (a panel resizing around it), so the click may have missed: wait for what moves it first (`until`, `page.idle()`).
- **Writing the PR?** `report experiment <exp> --format pr --update-pr <n>` puts the experiment's block in the PR body, led by its result card (or paste stdout, text only).
- **Is the rewrite faster?** One bench file, one target per implementation, the same cases (journeys) on both:
  - `bench <file> --base original --head rewrite` (`<target>[=<variant>]`: `rewrite=git:main`, `original=exe:/path`; `target:<name>` when a folder has the name).
  - Each target has a `name` (what every report calls it); `cwd: "../other-repo"` takes that repo's strobe.json for its builds and app profiles.
  - Both talk to one `fixture("<server> --port {port}", { seed })`: strobe starts it per block outside both apps; pass `{fixture.url}` in the targets' `args`, `env` or `files`.
    - `FixtureMismatch`: no loopback connection the fixture's processes answered (Unix sockets, proxies and containers count for none), a side served its port itself, or a side running its executable (or a copy) serves a port.
  - A declared `viewport` holds in every run-case block, `--check` too (app and native windows sized through the OS; macOS needs the accessibility permission); a window that keeps another size refuses `VariantMismatch` naming the side.
  - A headed page's viewport must fit the display: its work area less the browser's tabs and toolbar. Checked before the first block, in one window of the browser (none on a locked or in-use Mac: the first block checks it then; `plan` too, once strobe has seen that browser there); one that does not fit refuses `DisplayTooSmall` with the most this display shows (a 1470×956 MacBook Air shows at most 1470×838 of a page in Firefox). Declare a smaller viewport, use a larger display, or `--headless`.
    - A macOS native window is measured by its frame (title bar included): against a page, up to 32 points taller is the same window.
    - Launches keep the window each app opens; their sides must match each other.
  - Pick a primary both sides measure the same way: `cpu.work`, `run.duration`, `launch.ready`, `memory.app.private`; `main.work` or `ui.cpu` against another engine exits 2 (`NotComparable`).
  - Give each target a `ui` adapter for what differs: `ui: { timeline: ".scroll-view", openSession: ({ page }) => page.click("@row") }`; journeys say `page.scroll("@timeline", …)`, `await ctx.ui.openSession()`, `until: "@timeline"`, `"@row index=-1"` (the last).
  - A journey one side cannot run: `unsupported: { rewrite: "why" }` on the case, or just an `@name` its `ui` lacks (a parity gap: listed with the missing name, never run).
  - `expect: { same: ["@topRow"] }`: both sides must read it the same after setup, each run and each reset, or the series is refused `NotEquivalent`. Steps that differ entirely: `run: { original: fn, rewrite: fn }` (needs `expect.same`).
  - Same input on both sides: a page against a native app, or against a page in another engine (Chromium against Firefox), gets OS input like a native app; `page.act` on one side and `click` on the other, or any kind of call over 5% apart, is `InputMismatch`.
    - Run those headed, window in front and still: a headless page refuses input in its runs (`Headless`), and so does Safari (`InputMismatch`: a WebDriver session's window takes no OS input; compare Safari only with Safari). A window moved, resized or zoomed during a run refuses `InputMismatch`, as does input that never arrived (every key and button edge must).
    - Check a pair as its comparison measures it: `strobe bench x.bench.ts --base chromium --head firefox --check` (each case once on each, from outside when the engines differ). Keep waits in runs on `until: "frame"`/`"idle"`: selector waits read the app inside the run (`driver.reads`, `driver.read_ms`; `ObserverCost` past 8 reads a run or 2% of its time).
  - An `@name` no target's `ui` has is a typo (exit 2); one another target has is a gap, even when only met mid-run.
  - Give the app under test its own bundle id (macOS) through its build's own config overrides, so strobe may launch it beside your installed copy (`LiveChannel`).
  - `--check` runs every target's cases once and prints the parity matrix (ok, gap, what `expect.same` read, why journeys differ); `--check --head <target>` one side's; `--noworse 5%` asks "no worse anywhere".
  - Where each spends its time: `--why always` profiles each side once with its own engine's tools.
  - Read each side alone (`query <rec> threads`, `hotspots --by package`); `query a..b` across implementations is refused (`NotComparable`).
  - `report experiment <exp> --open`: both sides in two columns; a shared scale only where both come from one source.
  - Profiled launches are perturbed (a browser's profiler far more than a native sampler): compare totals from the timed pairs.
  - Over time: `track <file> --base original=exe:/path --head rewrite=git:main --schedule commits`, then `history <file>` and `report history <file> --format card`.

## Every performance claim in a PR comes from strobe

When a PR claims a performance change (faster, less CPU, fewer wakeups, less memory, smoother), its body carries strobe's evidence, and nothing else stands in for it:

1. **Discovery**: the strobe command and recording id that pointed at the problem, and what it showed.
2. **The change**: what the code does differently.
3. **Results**: the block `report experiment <exp> --format pr` writes, verbatim.
   - Put it in with `--update-pr <n>` (it replaces only its own block, so rerun it after a new experiment) or paste stdout.
   - The block is strobe's evidence and says so:
     - it starts with `<!-- strobe:report <exp> v1 begin sha256:… -->` and a quiet "strobe · <exp> · generated" line
     - and ends with an "end of generated report" line.
   - Visible:
     - one `#` hero line with the decided wins of the main metrics (a calm line or `Regression: …` without one)
     - a caution alert for anything decidedly worse
     - one proof line per series, at most one caveat
   - Folded: Details (metrics, pairs, profiles, method, also measured) and Rerun.
   - Never edit inside it (strobe refuses to update a hand-edited block): don't retype numbers, round them, drop rows, the setup line or the alert, or rewrite verdicts.
   - Write your own summary and reading of the numbers outside it, above or below; if a metric was refused or `unclear`, say so there too.
   - One block per experiment when a PR has several.
   - The card `--update-pr` attaches is the block's only colour and the first thing a reader sees.
     - It shows the same results as the headings (wins, what got worse beside them, or a calm "No measurable change" / "Not decided yet"), never more.
     - It helps a PR whose change is the point (a win to see at a glance, a regression to notice); it never replaces the numbers.
     - Never put a card or badge in a PR without the block.
     - `--format card` writes one for elsewhere (a release note, a chat message); `--format badge` a one-line badge.
4. **The bench**: the case's code, or a link to the bench file and case when it already exists.
   - A new bench case ships in the same PR as the first change it measures.

Never put hand-measured timings, single recordings or `--check` smoke numbers in a results table: label them as noisy discovery evidence, or leave them out.

## Notes

- Timing and attribution are separate blocks; never read timing from a `mixed` recording.
- `unclear` or `underpowered` means more pairs or a quieter machine, not "no change".
- Heap snapshots and traces can hold user content; views redact strings unless `--reveal`.
- Long benches keep running in the daemon: `query exp_… progress`.
  - Ctrl-C, SIGTERM or `strobe bench --stop <exp>` (a unique prefix works; it says what the bench was doing) stops one after its block, or at once while it waits for an unlock or the slot.
  - It writes what finished (exit 130; the PR table says incomplete).
  - `strobe bench --resume <exp>` goes on from the last finished pair.
  - `--unlock-wait 2h` caps how long a series that needs a presenting window waits for an unlock (then exit 124, resumable).
- Launches that fail under load (no debugging port, no DevTools reply) run again up to 3 times.
- A share primary stuck at its ceiling on both sides stops as `saturated`: choose another primary.
- If `strobe` isn't on PATH, use the CLI path from the MCP instructions.
- Progress goes to stderr.
  - Windows PowerShell 5.1 with `2>&1` or `*>` makes every stderr line an error and the shell exit 1 even when strobe exited 0.
  - Read `$LASTEXITCODE`, or pass `--progress stdout` (`STROBE_PROGRESS=stdout`).
- Recordings and `--check` say what the screen was doing (`screen  display off · session locked · dwm.exe 0.4% of a core`) and which commit a folder variant was at.
- `UnpacedSpin` warns when a native app presents far faster than the display refreshes (pacing broken, such as composited while locked): its CPU then describes a spin.

### Linux and macOS frames

- Frames on screen come from X Present (X11 on a real display, no compositing manager; Xvfb, compositors and Wayland are refused) and ScreenCaptureKit (needs Screen Recording).
- Both give screen times only, so `frames.interval.*` and `frames.dropped*` are refused there while counts and `interaction.*` stand.
- A macOS window the app excludes from screen capture (sharing type none: an overlay left out of recordings) gets no stream.
  - Frame metrics of a block that showed one are refused `NoPresentSource`, naming the window, not reported as 0.

### Linux and macOS permissions

- Driving a native app needs, on macOS, the Accessibility permission for the app strobe runs under (`strobe status` names it: Terminal, the agent's app, or `~/.local/bin/mesh` remotely).
- On Linux it needs the session's accessibility bus (at-spi2-core) and an X11 display.
- macOS 27 names the Accessibility pane "Device Control and Data Access"; strobe's messages name the right pane with its URL.
- Linux native stacks need `perf_event_paranoid` ≤ 2 or CAP_PERFMON; `status` says which.

### macOS quirks

- Sandboxed Mac apps (TextEdit) are refused: their data lives in your `~/Library`.
- macOS counts wakes per process, IO as disk bytes, no VRAM per process, and samples stacks with `/usr/bin/sample` (untimed).
- A macOS Cocoa app launches isolated only while its `~/Library` data does not exist (`LiveChannel` names the folder).
  - Its Documents, Application Support and caches land in the home (`CFFIXED_USER_HOME`); its preferences still go to your `~/Library/Preferences`.
- A copy that adds a file to your real Documents or Desktop stops its first launch per side (or a `--check`), naming it; later launches leave a `UserFolders` caution instead.
- A source build run as a bare executable with its data folder moved by an env var lists that folder in the profile's `moved`.

### Background priority

- Never bench under `taskpolicy -b` (or nice 19, `SCHED_IDLE`, Windows' idle priority or efficiency mode): the target inherits background priority.
  - So `BackgroundPriority` refuses frame and input-latency metrics (a frame primary refuses the bench up front); CPU metrics stay with a warning.

### macOS timer coalescing

- On macOS, a strobe started by a launchd job whose plist sets no `ProcessType` (a remote shell's or agent's daemon) runs with coalesced timers, and so does the app it launches.
- `TimerCoalescing` warns and `status` shows a `timers` line; compare such runs only with runs started the same way, or run from a terminal.
- strobe's own daemon starts from a LaunchAgent with `ProcessType Interactive` (`strobe up`; Linux: a systemd user unit), so MCP benches do not have this.
