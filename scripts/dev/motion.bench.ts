// Dev bench for reduced motion (not shipped). Run from the repo root:
//   strobe bench scripts/dev/motion.bench.ts --head . --check [--engine firefox|webkit] [--headless] --filter static
//   strobe bench scripts/dev/motion.bench.ts --head . --check --filter contract     (a native app's declared contract)
// `static` runs only where the page took its reduced-motion path (`motion: "reduce"`); `animated` is the
// same page as the OS leaves it (on a machine whose OS does not reduce motion); `contract` launches the fake
// native app, whose profile declares `motion.reduce` (scripts/dev/strobe.json), and ends once it says it got it.
import { bench, native, url } from "@hona/strobe"

const page = new URL("pages/motion.html", import.meta.url).href
const animation = (p: any): Promise<string> => p.eval(() => getComputedStyle(document.getElementById("box")!).animationName)

export default bench({
  targets: {
    reduced: url(page, { motion: "reduce" }),
    system: url(page),
    app: native("fake", { args: [import.meta.dir + "/fake-motion.ts"], home: "motion-home", window: false, motion: "reduce" }),
  },
  cases: {
    static: {
      targets: ["reduced"],
      primary: "run.duration",
      run: async ({ page }) => {
        const name = await animation(page)
        if (name !== "none") throw new Error(`the page animates (${name}): it did not take its reduced-motion path`)
      },
    },
    animated: {
      targets: ["system"],
      primary: "run.duration",
      run: async ({ page }) => {
        const name = await animation(page)
        if (name !== "spin") throw new Error(`the page shows ${name}, not its animation`)
      },
    },
    "first visit": { targets: ["reduced"], primary: "launch.ready", launch: { until: "#ready" } },
    contract: {
      targets: ["app"],
      primary: "launch.ready",
      launch: { until: { milestone: "reduced" }, milestones: { reduced: { match: "motion reduced, theme kept" } }, timeout: 20_000 },
    },
  },
})
