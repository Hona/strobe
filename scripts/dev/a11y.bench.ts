// Dev bench for the accessibility driver on a classic Win32 app (not shipped): finds elements through
// UI Automation, types, presses chords and clicks with posted messages. Run from the repo root:
//   strobe bench scripts/dev/a11y.bench.ts --head . --check
// Character Map's AutomationIds are its control ids: 104 the text to copy, 103 Select, 102 Copy.
// Cases measuring only CPU run on a locked session under --check; `latency` needs an unlocked one.
import { bench, native } from "@hona/strobe"

const clear = async ({ page }: any) => {
  await page.act("104", "setValue", { value: "" })
  await page.waitFor("104", { text: "", exact: true })
}

export default bench({
  targets: { charmap: native("C:/Windows/System32/charmap.exe") },
  async setup({ page }) {
    console.error("buttons", JSON.stringify(await page.query("role=button")))
    await page.waitFor("role=textbox")
  },
  cases: {
    query: { primary: "cpu.work", run: async ({ page }) => void (await page.query("role=button")) },
    // Upper case and symbols: Shift in the key state the app's thread reads. The text box is its own
    // window, so on a locked session (no click can be aimed) its keys go straight to it.
    type: {
      primary: "cpu.work",
      metrics: ["ui.cpu", "ui.wakes"],
      async run({ page }) {
        await page.type("104", "Ab+1 x", { until: { selector: "104", text: "Ab+1 x", exact: true } })
      },
      reset: clear,
    },
    // Chords: Shift+Home selects back to the start, Backspace deletes the selection.
    chord: {
      primary: "cpu.work",
      async run({ page }) {
        await page.type("104", "hello", { until: { selector: "104", text: "hello", exact: true } })
        await page.press("Shift+Home", { selector: "104" })
        await page.press("Backspace", { selector: "104", until: { selector: "104", text: "", exact: true } })
      },
      reset: clear,
    },
    // A click on Select copies the grid's selected character into the text box.
    click: { primary: "cpu.work", run: ({ page }) => page.click("103", { until: { selector: "104", text: "!" } }), reset: clear },
    act: { primary: "cpu.work", run: ({ page }) => page.act("104", "setValue", { value: "xyz", until: { selector: "104", text: "xyz" } }), reset: clear },
    latency: { primary: "interaction.p95", metrics: ["frames.interval.p95", "ui.cpu"], run: ({ page }) => page.type("104", "abcdef", { delay: 80 }), reset: clear },
  },
})
