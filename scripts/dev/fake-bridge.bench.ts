// Dev bench for the native bridge (not shipped): a fake app (bun running fake-bridge.ts, copied by strobe.json's
// build so it is not the bun that runs the bench) speaks native-bridge-v1. Run from this folder:
//   strobe bench fake-bridge.bench.ts --head . --check
import { bench, headless, native } from "@hona/strobe"

const fake = { args: [import.meta.dir + "/fake-bridge.ts"] }

export default bench({
  targets: {
    win: native("fake", { ...fake, viewport: { width: 900, height: 700 } }),
    fast: headless("fake", fake),
  },
  cases: {
    "pane-switch": {
      primary: "ui.work",
      metrics: ["ui.frames", "ui.layout", "count.commits", "cpu.wakes"],
      async run({ page, phase }) {
        await phase("open", () => page.click("nav-history", { until: "history-pane" }))
        await page.click("nav-general", { until: "general-pane" })
      },
    },
    scroll: {
      targets: ["win"],
      primary: "ui.interval.p95",
      metrics: ["ui.work"],
      run: ({ page }) => page.scroll("history-pane", { by: 3000, speed: 3000 }, { until: "idle" }),
    },
    idle: { primary: "cpu.wakes", metrics: ["cpu.work", "ui.work"], run: ({ page }) => page.wait(1000) },
    refused: { targets: ["win"], primary: "ui.work", metrics: ["frames.presented.p95", "main.work"], run: ({ page }) => page.click("nav-history", { until: "history-pane" }), reset: ({ page }) => page.click("nav-general", { until: "general-pane" }) },
    unsupported: { targets: ["win"], primary: "cpu.work", run: ({ page }) => page.down("w") },
    launch: { targets: ["win"], primary: "launch.ready", metrics: ["launch.settled"], launch: { until: "app-ready" } },
  },
})
