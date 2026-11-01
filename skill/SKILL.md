---
name: strobe
description: Measure and explain performance of web pages, Electron apps, Bun or Node processes and any process. Use when the user asks whether a change is faster, why something is slow, janky or heavy, where memory goes, what the CPU or GPU is doing, or to write a benchmark.
---

# Strobe

strobe records a target on one timeline and answers with verdicts and handles (`rec_…`, `exp_…`) to drill into. It refuses measurements it cannot defend: a refused run is an answer.

## Reference

    strobe status [--calibrate]                    environment, validity preflight, recent handles
    strobe record <target> [--probes timing|attribution|memory] [--for 10s] [--headless] [--served <dir>]
    strobe bench <file.bench.ts> --head <variant> [--base <variant> | --aa | --check] [--filter <case>] [--target <name>]
                 [--params k=v|all] [--primary <metric>] [--pairs auto|N] [--runs 5] [--power keep|high]
                 [--why auto|always|never] [--trace lean|full] [--probes memory --repeat 10] [--bisect good..bad]
        variant: a directory, a git ref, app:<id>, exe:<path>
    strobe query <rec_… | a..b | exp_…> <view> [args]    views: summary frames threads hotspots alloc layout shifts gc heap leaks memory insights sql
        hotspots and alloc take --by function|file|package; memory takes --by process|layer|allocator and --at <i>;
        a..b works with heap and memory; exp_… takes summary, progress and insights; --reveal shows heap strings
    strobe report experiment|recording <handle> [--format html|md]    writes a file; show it in the Review pane

MCP: `status`, `record`, `bench`, `query`, `report`: same names, arguments and JSON as the CLI.

Targets: `https://…` · `cdp:<port>` · `app:<id>` (a profile in strobe.json `apps`) · `exe:<path>` · `cdp:ws://…` · `file:<path>` (import a trace, .cpuprofile or .heapsnapshot)

Probe sets: `timing` (default) · `attribution` (CPU stacks, allocations, forced layouts, GC, a heap snapshot; its times are perturbed) · `memory` (three heap snapshots for leaks, no trace)

Exit codes: `0` · `1` error · `2` usage · `3` needs a human · `4` refused (invalid; the reason is in the result) · `5` inconclusive · `124` timeout · `255` unreachable

## A bench file

    import { bench, web, app } from "@hona/strobe-bench"
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

`page`: `goto` (relative paths resolve against the page) `click` `hover` `move(x, y)` `type` (one key event per character; `delay` ms between) `press` (`"ControlOrMeta+Shift+P"`) `scroll` `waitFor` (`{ text, exact }`; `:focus` works) `idle` `wait`; input calls take `{ until: "idle" | "frame" | selector | { selector, text, exact } }`. `reset` puts state back between runs and is never measured. Playwright only in `prepare` on `web` targets; what `prepare` returns is `ctx.prepared` in `setup`, `run` and `reset`, and it keeps working for the whole block. Mock APIs with `ctx.mock(pattern, handler)` in `prepare`: Playwright's `page.route` turns the HTTP cache off, so reloads refetch every script. `teardown` (target or file) stops what `prepare` started. Never call `page.eval` in `run`.

More in a bench file:
- `launch: { until, milestones?, profile?, before?, inspect? }` instead of `run`: one cold launch per block, timed from spawn to `until` (`launch.ready`); milestones `{ match: "text" | /re/, in: "output" | "page" | "<log glob under the home>" }` become `launch.<name>`; `launch.painted`/`launch.settled` from the window's pixels.
- `command("bun run dev:desktop", { cwd, env: { X: "{home}" }, unset: ["APP_*"] })`: an app started by a dev command, attached where it prints `DevTools listening on ws://…`; `before(ctx)` deletes caches for a cold start.
- `script("src/parse.ts")`: a Bun or Node module per block; cases `await ctx.call("parse", input)`, primary `cpu.work`.
- Exact counters: `ctx.count(name)` in `run`, `globalThis.__strobeCount?.(name)` in the app → `count.<name>`; `net.requests{/api/*/message}` and `net.bytes`; `layout.shift` (then `query <rec> shifts` names what moved). A stable count is compared exactly.
- `params: { files: { run: [100, 10_000], more: [1, 1_000] } }`: `more` only with `--params all`. `targets: ["web"]` on a case limits where it runs.
- `bun add -d @hona/strobe-bench` gives editors and `tsc` the types; strobe runs its own copy either way.

Primary metric: `main.work` for "does this cost less" (continuous; durations move in whole vsyncs), `main.busy` for scrolling and streaming, `interaction.p95` for typing and clicks, `frames.dropped.pct` when frames do drop, `launch.ready` for launches, `cpu.work` for scripts. Fewer than 6 pairs never gets a verdict. Validate a new file with `--check` first: each case once, pass or fail with a screenshot.

Everything about the app under test lives in its repo's `strobe.json`: `build` and `serve` for web targets, and `apps` profiles for Electron apps (`exe` per OS, `env`, `unset`, `dirs`, `files`, `guard` for the user's own state, `protocols`, `extends`). Strobe itself knows no product. If an isolated launch disturbs the user's state, strobe stops: add what leaked to the profile.

## Recipes

Did my change help? `bench <file> --base origin/dev --head .` and read the verdict per series.
Why is it slow? `bench … --why always` (or `record --probes attribution`), then on the attribution recording: `query hotspots` (CPU by function, full stacks, original file:line:column through source maps), `query layout` (forced layouts by the call that forced them), `query alloc` (who allocates), `query gc`. `main.script`, `main.layout` and `main.style` in the bench result say where main-thread time went, with intervals.
What stands out? `query <rec or exp> insights`: forced layout, GC pressure, long tasks, work while idle, detached DOM, background load, unresolved time, and for an experiment the biggest base/head movers, each with the command that shows it.
Where is the memory? `query <rec> memory` (per process; `--by layer` or `--by allocator` for Chromium's own accounting).
Which commit moved it? `bench <file> --filter <case> --bisect <good>..<bad>` (one series; each step is a full experiment).
Which objects are alive? `query <rec> heap` (classes by retained size, detached DOM, retainer paths); `query <base rec>..<head rec> heap` compares two.
Does a case leak? `bench <file> --filter <case> --head . --probes memory --repeat 10`: growth per run of the JS heap and DOM with an interval, and the objects from the first half still alive at the end (`query <rec> leaks`).
Is an app leaking over time? `record <target> --probes memory --for 60s` (do the suspect thing meanwhile, or point it at a page that does), then `query <rec> leaks`: objects from the first half still alive at the end, with what keeps them.
Is the method sound here? `bench <file> --aa --head .`.

## Notes

- Timing and attribution are separate blocks; never read timing from a `mixed` recording.
- `unclear` or `underpowered` means more pairs or a quieter machine, not "no change".
- Heap snapshots and traces can hold user content; views redact strings unless `--reveal`.
- Long benches keep running in the daemon: `query exp_… progress`.
- If `strobe` isn't on PATH, use the CLI path from the MCP instructions.
