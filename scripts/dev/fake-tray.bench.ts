// Dev bench for a windowless (tray) native app while the session may be locked (not shipped). Run from the repo root:
//   strobe bench scripts/dev/fake-tray.bench.ts --head . --check
//   strobe bench scripts/dev/fake-tray.bench.ts --head . --probes memory --repeat 4 --filter leaky   (a native leak run)
import { bench, native } from "@hona/strobe"

export default bench({
  targets: {
    tray: native("fake", { args: [import.meta.dir + "/fake-tray.ts"], window: false }),
    leaky: native("fake", { args: [import.meta.dir + "/fake-tray.ts", "leak"], window: false }),
  },
  cases: {
    idle: { primary: "cpu.wakes", metrics: ["cpu.work", "memory.app.private", "memory.app.host", "gpu.vram", "gpu.process.3d"], run: ({ page }) => page.wait(1000) },
    launch: { primary: "launch.ready", metrics: ["cpu.work"], launch: { until: { milestone: "up" }, milestones: { up: { match: "fake ready" } } } },
  },
})
