// Dev bench for the accessibility driver on Linux (not shipped): a GTK 3 app driven through AT-SPI and
// XTest. Run from the repo root on an X11 display with a session bus (Xvfb works; frames are refused
// there, it has no display):
//   strobe bench scripts/dev/atspi.bench.ts --head . --check
import { bench, native } from "@hona/strobe"

export default bench({
  targets: { gtk: native(`${import.meta.dir}/gtk-app.py`, { viewport: { width: 640, height: 480 } }) },
  async setup({ page }) {
    console.error("buttons", JSON.stringify(await page.query("role=button")))
    await page.waitFor("add")
  },
  cases: {
    query: { primary: "cpu.work", run: async ({ page }) => void (await page.query("role=button")) },
    click: { primary: "cpu.work", metrics: ["ui.cpu", "ui.wakes", "interaction.p95"], run: ({ page }) => page.click("add", { until: { selector: "count", text: "rows: 1", exact: false } }) },
    type: {
      primary: "cpu.work",
      run: ({ page }) => page.type("field", "Ab+1 x", { until: { selector: "echo", text: "echo: Ab+1 x", exact: true } }),
      reset: async ({ page }) => {
        await page.act("field", "setValue", { value: "" })
        await page.waitFor("echo", { text: "echo: ", exact: true })
      },
    },
    chord: {
      primary: "cpu.work",
      async run({ page }) {
        await page.type("field", "hello", { until: { selector: "echo", text: "echo: hello", exact: true } })
        await page.press("Shift+Home", { selector: "field" })
        await page.press("Backspace", { selector: "field", until: { selector: "echo", text: "echo: ", exact: true } })
      },
    },
    toggle: { primary: "cpu.work", run: ({ page }) => page.act("check", "toggle", { until: "idle" }) },
    scroll: { primary: "cpu.work", metrics: ["scroll.latency.p95"], run: ({ page }) => page.scroll("scroll", { by: 600 }, { until: "idle" }) },
    wheel: { primary: "cpu.work", run: ({ page }) => page.wheel("scroll", { dy: -300 }, { until: "idle" }) },
    misc: {
      primary: "cpu.work",
      async run({ page }) {
        await page.hover("check")
        await page.move(20, 20)
        await page.window("focus")
        await page.down("Shift")
        await page.up("Shift")
        await page.press("Control+a", { selector: "field" })
        await page.idle()
      },
    },
    frames: { primary: "cpu.work", metrics: ["frames.presented.count", "interaction.p95"], run: ({ page }) => page.click("add", { until: "frame" }) },
    // Every element, with rects in the window's client area (what page.move takes) and actionable by hit test.
    tree: {
      primary: "cpu.work",
      async run({ page }) {
        const all = await page.query()
        console.error("tree", JSON.stringify(all.filter((e) => e.key)))
        const add = all.find((e) => e.key === "add")
        if (!add?.actionable || add.rect[0] < 0 || add.rect[1] < 0 || add.rect[0] > 640) throw new Error(`add: ${JSON.stringify(add)}`)
        await page.move(add.rect[0] + add.rect[2] / 2, add.rect[1] + add.rect[3] / 2)
      },
    },
    // The window resized and maximized through the window manager, and restored in reset.
    window: {
      primary: "cpu.work",
      async run({ page }) {
        await page.window({ resize: { width: 800, height: 500 } })
        await page.window("maximize")
        await page.click("check", { until: "idle" })
      },
      reset: async ({ page }) => {
        await page.window("restore")
        await page.act("check", "toggle", { until: "idle" })
      },
    },
  },
})
