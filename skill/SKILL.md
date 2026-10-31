---
name: strobe
description: Measure and explain performance of web pages, Electron apps, Bun or Node processes and any process. Use when the user asks whether a change is faster, why something is slow, janky or heavy, where memory goes, what the CPU or GPU is doing, or to write a benchmark.
---

# Strobe

strobe records a target on one timeline and answers with verdicts and handles (`rec_…`, `exp_…`, `fn_…`) to drill into. It refuses measurements it cannot defend: a refused run is an answer.

## Reference

    strobe status [target] [--calibrate]          environment, validity preflight, attachable targets
    strobe record <target> [--probes <set>] [--for 10s | --bench <file>#<case>]
    strobe bench <file.bench.ts> --head <variant> [--base <variant>] [--filter <case>] [--target <name>]
                 [--params k=v] [--primary <metric>] [--aa] [--pairs auto|N] [--runs 5] [--power keep|high]
                 [--why auto|always|never] [--trace lean|full]
                 [--probes memory --repeat 10] [--bisect a..b]
        variant: a directory, a git ref, app:<id>, exe:<path>
    strobe query <rec_… | a..b | exp_…> <view> [args]    views: summary frames threads hotspots stack
                                                         memory heap leaks gpu cpu insights timeline sql
    strobe report <view> <handle…> [--format md|html]    writes a file; show it in the Review pane

MCP: `status`, `record`, `bench`, `query`, `report`: same names, arguments and JSON as the CLI.

Targets: `https://…` · `cdp:<port>` · `app:<id>` (a profile in strobe.json `apps`) · `exe:<path>` · `node:<port>` · `pid:<n>` · `file:<path>` · `<target>@<device>` (mesh)

Probe sets: `timing` (default) · `attribution` (CPU stacks, allocation, invalidations, counters) · `memory` (census, heap snapshots) · `deep` (native stacks)

Exit codes: `0` · `1` error · `2` usage · `3` needs a human · `4` refused (invalid; the reason is in the result) · `5` inconclusive · `124` timeout · `255` unreachable

## A bench file

    import { bench, web, app } from "@strobe/bench"
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

`page`: `goto` (relative paths resolve against the page) `click` `type` (one key event per character; `delay` ms between) `press` (`"ControlOrMeta+Shift+P"`) `scroll` `waitFor` (`{ text }` to wait for content) `idle` `wait`; input calls take `{ until: "idle" | "frame" | selector }`. `reset` puts state back between runs and is never measured. Playwright only in `prepare` on `web` targets; what `prepare` returns is `ctx.prepared` in `setup`, `run` and `reset`, and it keeps working (routes, mocks) for the whole block. Never call `page.eval` in `run`.

Primary metric: `main.busy` for scrolling and streaming on fast machines (frames rarely drop at normal speed), `interaction.p95` for typing and clicks, `frames.dropped.pct` when frames do drop. Fewer than 6 pairs never gets a verdict.

Everything about the app under test lives in its repo's `strobe.json`: `build` and `serve` for web targets, and `apps` profiles for Electron apps (`exe` per OS, `env`, `unset`, `dirs`, `files`, `guard` for the user's own state, `protocols`, `extends`). Strobe itself knows no product. If an isolated launch disturbs the user's state, strobe stops: add what leaked to the profile.

## Recipes

Did my change help? `bench <file> --base origin/dev --head .` and read the verdict per series.
Why is it slow? `record --probes attribution`, then `query frames`, then `query stack <id>`.
Where is the memory? `record --probes memory`, then `query memory --by process`.
Is it leaking? `bench <file> --filter <case> --head . --probes memory --repeat 10`, then `query leaks`.
An installed app: `status app:<id>` shows whether its bridge is on.
Which commit regressed? `bench <file> --bisect <good>..<bad>`.
Is the method sound here? `bench <file> --aa --head .`.

## Notes

- Timing and attribution are separate blocks; never read timing from a `mixed` recording.
- `unclear` or `underpowered` means more pairs or a quieter machine, not "no change".
- Heap snapshots and traces can hold user content; views redact strings unless `--reveal`.
- Long benches keep running in the daemon: `query exp_… progress`.
- If `strobe` isn't on PATH, use the CLI path from the MCP instructions.
