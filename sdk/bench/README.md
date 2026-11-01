# @hona/strobe-bench

Types and helpers for [strobe](https://github.com/Hona/strobe) bench files.

strobe runs a bench file with its own copy of this package, so a bench file runs without installing
anything. Install the package for your editor and `tsc`: the types catch a renamed option before a
long run.

```sh
bun add -d @hona/strobe-bench
```

```ts
import { bench, web } from "@hona/strobe-bench"

export default bench({
  targets: { app: web({ url: "/" }) },
  cases: {
    "open settings": {
      primary: "main.work",
      run: (ctx) => ctx.page.click("[data-action=settings]", { until: "[role=dialog]" }),
      reset: (ctx) => ctx.page.press("Escape", { until: "idle" }),
    },
  },
})
```

```sh
strobe bench perf/settings.bench.ts --check            # each case once: does it work?
strobe bench perf/settings.bench.ts --base git:main    # is this branch faster?
```

- **Targets:** `web()`, `url()`, `app()`, `exe()`, `command()` (a dev command that launches an app)
  and `script()` (a Bun or Node module whose exports cases call).
- **Cases:** `run` measures work inside an open target. `launch` measures the launch itself, from
  process spawn to a selector, with milestones on the way.

The package version matches the strobe version whose SDK it describes.
