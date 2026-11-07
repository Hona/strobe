// Dev bench for many cases per launch on a script and a windowless native app (not shipped). `exits` ends the
// script's host process: it fails alone and the cases after it get a fresh host. Run from the repo root:
//   strobe bench scripts/dev/batch-more.bench.ts --head . --check --batch 4
import { bench, native, script } from "@hona/strobe"

const cases: Record<string, any> = {}
for (let i = 1; i <= 6; i++)
  cases[`work-${i}`] = {
    targets: ["harness"],
    primary: "cpu.work",
    run: ({ call }: any) => call("work", i),
  }
cases.exits = { targets: ["harness"], primary: "cpu.work", run: ({ call }: any) => call("exit") }
cases.keeps = {
  targets: ["harness"],
  primary: "cpu.work",
  metrics: ["memory.app.private"],
  run: ({ call }: any) => call("keep", 1000),
  reset: ({ call }: any) => call("clear"),
}
for (let i = 1; i <= 4; i++) cases[`idle-${i}`] = { targets: ["tray"], primary: "cpu.work", run: ({ page }: any) => page.wait(200 * i) }

export default bench({
  targets: {
    harness: script("./batch-harness.ts"),
    tray: native("fake", { args: [import.meta.dir + "/fake-tray.ts"], window: false }),
  },
  cases,
})
