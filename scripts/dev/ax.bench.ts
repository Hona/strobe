// Dev bench for the accessibility driver on macOS (not shipped): a small Cocoa app driven through the AX
// API and Quartz events posted to its process. Needs the Accessibility permission (and Screen Recording
// for frames) for the app strobe runs under; `strobe status` says which. TextEdit and Apple's other
// sandboxed apps keep their data in your ~/Library, so strobe does not launch them. Run from the repo root:
//   swiftc -O scripts/dev/cocoa-app.swift -o /tmp/strobe-cocoa-app
//   strobe bench scripts/dev/ax.bench.ts --head . --check
import { bench, native } from "@hona/strobe"

export default bench({
  targets: { cocoa: native("/tmp/strobe-cocoa-app", { viewport: { width: 640, height: 480 } }) },
  async setup({ page }) {
    console.error("buttons", JSON.stringify(await page.query("role=button")))
    await page.waitFor("add")
  },
  cases: {
    query: { primary: "cpu.work", run: async ({ page }) => void (await page.query("role=button")) },
    click: { primary: "cpu.work", metrics: ["ui.cpu", "interaction.p95"], run: ({ page }) => page.click("add", { until: { selector: "count", text: "rows: 1" } }) },
    type: {
      primary: "cpu.work",
      metrics: ["interaction.p95"],
      run: ({ page }) => page.type("field", "Ab+1 x", { until: { selector: "echo", text: "echo: Ab+1 x", exact: true } }),
      reset: async ({ page }) => {
        await page.act("field", "setValue", { value: "" })
        await page.waitFor("field", { text: "", exact: true })
      },
    },
    chord: {
      primary: "cpu.work",
      async run({ page }) {
        await page.type("field", "hello", { until: { selector: "echo", text: "echo: hello", exact: true } })
        await page.press("Meta+a", { selector: "field" })
        await page.press("Backspace", { selector: "field", until: { selector: "echo", text: "echo: ", exact: true } })
      },
    },
    toggle: { primary: "cpu.work", run: ({ page }) => page.act("check", "toggle", { until: "idle" }) },
    scroll: { primary: "cpu.work", metrics: ["scroll.latency.p95"], run: ({ page }) => page.scroll("scroll", { by: 600 }, { until: "idle" }) },
    wheel: { primary: "cpu.work", run: ({ page }) => page.wheel("scroll", { dy: -300 }, { until: "idle" }) },
    frames: { primary: "cpu.work", metrics: ["frames.presented.count", "interaction.p95"], run: ({ page }) => page.click("add", { until: "frame" }) },
  },
})
