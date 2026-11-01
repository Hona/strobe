# @hona/strobe

[strobe](https://github.com/Hona/strobe): measure and explain performance. This package is the `strobe` command
and the types and helpers for its bench files, so a project pins one strobe version for everyone who runs its
benches, people, agents and CI alike.

```sh
bun add -d @hona/strobe
```

```ts
import { bench, web } from "@hona/strobe"

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
bunx strobe bench perf/settings.bench.ts --check            # each case once: does it work?
bunx strobe bench perf/settings.bench.ts --base git:main    # is this branch faster?
```

- **Platforms:** the binary comes from `@hona/strobe-<platform>-<arch>`, installed with this package for your
  machine only. Windows x64 so far.
- **One SDK per binary:** strobe runs a bench file with the SDK it was built with, so the types you import and
  the runner always match.
- **Old name:** `@hona/strobe-bench` re-exports this package. Import `@hona/strobe` instead.
