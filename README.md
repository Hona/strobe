# strobe

Measure and explain performance: a web page, an Electron app, a Bun or Node process, or any process. strobe records everything on one clock, refuses measurements it cannot defend, and gives you, or your agent, a verdict with handles to drill into. It knows no product: what an app under test needs (how to build, serve or launch it in isolation) lives in that app's own `strobe.json`.

> Pre-release, Windows first. The design is in [SPEC.md](SPEC.md); what is built is listed in the skill (`skill/SKILL.md`) and `strobe --help`.

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
import { bench, web } from "@strobe/bench"

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

Built like [mesh](https://github.com/Hona/mesh): one small native binary, a per-user daemon, CLI and MCP with the same names and JSON, and a built-in skill for opencode.
