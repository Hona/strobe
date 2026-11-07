// Dev bench for page.query and page.window on pages (not shipped). `query` lists the page's elements and
// checks what each reads (roles, names, states, a password by its length, actionable by hit test, paths that
// find the element again, a shadow root's button), then clicks every actionable button by its path. `window`
// resizes and maximizes a headed page's window through the OS and restores it in reset (headless: refused
// Headless); `too-big` asks for more than any display shows (DisplayTooSmall). `setup-size` restores to the size
// setup chose, and `left-sized` leaves the window maximized: a declared viewport is held to the window as setup
// began, so neither is refused VariantMismatch. Run from the repo root:
//   strobe bench scripts/dev/tree.bench.ts --head . --check [--headless] [--engine firefox]
import { bench, url } from "@hona/strobe"

const tree = new URL("pages/tree.html", import.meta.url).href
const clear = () => {
  for (const el of document.querySelectorAll("[data-clicked]")) el.removeAttribute("data-clicked")
  for (const b of document.getElementById("host")?.shadowRoot?.querySelectorAll("button") ?? []) b.removeAttribute("data-clicked")
  ;(document.getElementById("more") as HTMLDetailsElement).open = false
}

const viewport = () => [innerWidth, innerHeight]
const near = ([w, h]: number[], want: [number, number]) => Math.abs(w - want[0]) <= 2 && Math.abs(h - want[1]) <= 2

export default bench({
  targets: { tree: url(tree), sized: url(tree, { viewport: { width: 1000, height: 700 } }) },
  // `setup-size`: setup picks the size the runs restore to.
  async setup({ page, case: c }) {
    if (c === "setup-size") await page.window({ resize: { width: 900, height: 600 } })
  },
  cases: {
    query: {
      targets: ["tree"],
      primary: "cpu.work",
      async run({ page }) {
        const all = await page.query()
        console.error("query", JSON.stringify(all))
        const by = (k: string) => all.find((e) => e.key === k)
        const want: [string, boolean][] = [
          ["a heading", all.some((e) => e.role === "heading" && e.name === "Tree")],
          ["a link", by("top-link")?.role === "link"],
          ["one actionable", by("one")?.actionable === true && by("one")?.enabled === true],
          ["a named button", all.some((e) => e.role === "button" && e.name === "Second button" && e.key === "")],
          ["off disabled", by("off")?.enabled === false && by("off")?.actionable === false],
          ["under covered", by("under")?.actionable === false && by("under")?.enabled === true],
          ["far below the fold", by("far")?.actionable === false],
          ["check checked", by("check")?.checked === true && by("check")?.role === "checkbox"],
          ["menu collapsed", by("menu")?.expanded === false],
          ["details collapsed", by("more")?.expanded === false],
          ["password redacted", by("secret")?.value === "8 characters"],
          ["select value", by("pick")?.value === "b" && by("pick")?.role === "combobox"],
          ["no hidden button", !all.some((e) => e.name === "Hidden")],
          ["shadow button", all.some((e) => e.name === "Shadow" && e.path.includes(">>> :host >") && e.actionable === true)],
          ["scrolled out of its box", by("inbox")?.actionable === false && by("inbox")?.enabled === true],
          ["rects in the viewport", all.filter((e) => e.actionable).every((e) => e.rect[0] >= 0 && e.rect[1] >= 0 && e.rect[2] > 0)],
        ]
        const failed = want.filter(([, ok]) => !ok).map(([what]) => what)
        if (failed.length) throw new Error(`query: ${failed.join(", ")}`)
        const buttons = await page.query("role=button", { actionable: true })
        if (buttons.some((b) => !b.actionable) || buttons.length < 4) throw new Error(`actionable: ${JSON.stringify(buttons)}`)
        for (const b of buttons) await page.click(b.path, { until: `${b.path}[data-clicked]` })
      },
      reset: ({ page }) => page.eval(clear),
    },
    window: {
      targets: ["tree"],
      primary: "cpu.work",
      async run({ page }) {
        await page.window({ resize: { width: 900, height: 600 } }, { until: "frame" })
        const [w, h] = await page.eval(() => [innerWidth, innerHeight])
        if (Math.abs(w - 900) > 2 || Math.abs(h - 600) > 2) throw new Error(`resize: the viewport is ${w}x${h}`)
        await page.window("maximize")
        await page.click("#one", { until: "#one[data-clicked]" })
      },
      reset: async ({ page }) => {
        await page.window("restore")
        await page.eval(clear)
      },
    },
    "too-big": { targets: ["tree"], primary: "cpu.work", run: ({ page }) => page.window({ resize: { width: 9000, height: 9000 } }) },
    // Restore goes back to setup's size, and the declared viewport is held to the window as setup began.
    "setup-size": {
      targets: ["sized"],
      primary: "cpu.work",
      run: ({ page }) => page.window("maximize"),
      reset: async ({ page }) => {
        await page.window("restore")
        const now = await page.eval(viewport)
        if (!near(now, [900, 600])) throw new Error(`restore: the viewport is ${now.join("x")}, not setup's 900x600`)
      },
    },
    // A run that leaves the window sized (no reset to size it back) is no viewport mismatch.
    "left-sized": { targets: ["sized"], primary: "cpu.work", resetCheck: false, run: ({ page }) => page.window("maximize") },
  },
})
