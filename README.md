# strobe

strobe makes your app faster, and proves it. Your agent finds what's slow, fixes it, and knows for certain it worked, with numbers it can't fake.

One tool for web, Electron and native apps on Windows, macOS and Linux. It measures what your users actually feel and explains it all the way down: the line of code, the native stack, every thread, wake-up, allocation, frame, GPU and byte of VRAM.

```ts
// perf/scroll.bench.ts
import { bench, url } from "@hona/strobe"

export default bench({
  targets: { app: url("http://localhost:3000") },
  cases: {
    scroll: {
      primary: "main.work",
      run: ({ page }) => page.scroll("main", { by: 4000 }),
      reset: ({ page }) => page.scroll("main", { by: -4000 }),
    },
  },
})
```

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/card-dark.png">
  <img alt="44% less CPU time, 78% less memory, better in 8 of 8 pairs" src="docs/card-light.png">
</picture>

Ask your agent: *"did this branch make scrolling faster?"*

**Agent quick start:** read [skill/SKILL.md](skill/SKILL.md). `strobe up` installs it as the `strobe` skill.

What your agent runs:

```sh
bun add -d @hona/strobe && bunx strobe up                      # once per machine
strobe bench perf/scroll.bench.ts --base origin/main --head .   # did the branch help?
strobe report experiment exp_9b3b26 --format pr --update-pr 42  # the card above, in the PR
strobe record app:desktop --for 10s                             # what is the installed app doing?
strobe query rec_3f9a hotspots                                  # where did the time go?
strobe query rec_3f9a..rec_7c1d hotspots                        # what got slower between two recordings?
strobe record app:desktop --probes memory --for 60s             # is it leaking?
strobe look app:desktop                                         # what's on screen, as strobe sees it
strobe open rec_3f9a                                            # the whole recording in Perfetto
```

`app:desktop` is a profile in your repo's `strobe.json`: how to build and launch the app in a throwaway home, so benches never touch your real data. `strobe status` says what your machine can measure and which permission is missing.

Design: [SPEC.md](SPEC.md)
