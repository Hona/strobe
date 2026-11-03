// Dev smoke bench for native targets measured from outside (not shipped): a classic Win32 app.
import { bench, native } from "@hona/strobe"

export default bench({
  targets: { app: native("C:/Windows/System32/charmap.exe") },
  cases: {
    idle: { primary: "cpu.wakes", metrics: ["cpu.work", "io.read", "gpu.process.3d"], run: ({ page }) => page.wait(2000) },
    launch: { primary: "launch.ready", metrics: ["launch.painted", "cpu.work"], launch: { until: { window: "settled" } } },
    refused: { primary: "cpu.work", metrics: ["frames.presented.p95"], run: ({ page }) => page.wait(500) },
  },
})
