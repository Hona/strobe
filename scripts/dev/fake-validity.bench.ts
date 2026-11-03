// Dev bench for native validity (not shipped): the fake bridge told to misbehave. Run from the repo root:
//   strobe bench scripts/dev/fake-validity.bench.ts --head . --check
import { bench, headless } from "@hona/strobe"

const fake = (env: Record<string, string>) => headless("fake", { args: [import.meta.dir + "/fake-bridge.ts"], env })
const click = async ({ page }: any) => {
  await page.click("nav-history", { until: "history-pane" })
  await page.click("nav-general", { until: "general-pane" })
}

export default bench({
  targets: {
    debug: fake({ FAKE_DEBUG: "1" }),
    software: fake({ FAKE_GPU: "Microsoft Basic Render Driver" }),
    skipped: fake({ FAKE_SKIP: "3" }),
    spin: fake({ FAKE_UNPACED: "40" }),
  },
  cases: {
    work: { primary: "ui.work", metrics: ["ui.frames", "ui.interval.p95", "count.glyphs_shaped"], run: click },
    display: { targets: ["debug"], primary: "ui.work", metrics: ["gpu.process.3d"], run: click },
  },
})