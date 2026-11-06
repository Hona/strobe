// Dev bench for typing at rest (not shipped): `page.type` into a search field, one key at a time, in a
// native Cocoa app (macOS) and in a page (every engine). Each key is an `interaction.*` sample only when
// the target was at rest when it went out (SPEC 7.4). The app draws for 100 ms after each key, and its
// caret mark blinks (one frame every 530 ms), as a focused page field's caret does.
// Run from the repo root:
//   swiftc -O scripts/dev/type-app.swift -o /tmp/strobe-type-app     (macOS, for the native target)
//   strobe bench scripts/dev/type.bench.ts --head . --check --target cocoa      (or firefox, chromium, webkit)
// `type` prints how many of its 9 keys were samples; `strobe query <rec> frames` says how many came
// mid-drawing. `caret` counts what a focused field draws in 3 s with no input.
import { bench, native, url } from "@hona/strobe"

const page = `<!doctype html><title>type</title>
<style>body{font:14px system-ui;margin:8px}input{width:300px;font:14px system-ui}</style>
<input id="field"><p id="echo">echo: </p>
<script>field.addEventListener("input", () => { echo.textContent = "echo: " + field.value })</script>`

const viewport = { width: 640, height: 480 }
const text = "search me"
const echo = (cocoa: boolean, s: string) => (cocoa ? { selector: "echo", text: `echo: ${s}`, exact: true } : { selector: "#echo", text: `echo: ${s}`, exact: true })
const clear = async ({ page, target }: any) => {
  if (target === "cocoa") {
    await page.act("field", "setValue", { value: "" })
    await page.waitFor("field", { text: "", exact: true })
  } else {
    await page.eval(() => {
      const f = document.querySelector("#field") as HTMLInputElement
      f.value = ""
      document.querySelector("#echo")!.textContent = "echo: "
    })
  }
}

export default bench({
  targets: {
    cocoa: native("/tmp/strobe-type-app", { viewport }),
    chromium: url(`data:text/html,${encodeURIComponent(page)}`, { viewport, engine: "chromium" }),
    firefox: url(`data:text/html,${encodeURIComponent(page)}`, { viewport, engine: "firefox" }),
    webkit: url(`data:text/html,${encodeURIComponent(page)}`, { viewport, engine: "webkit" }),
  },
  cases: {
    // Nine keys, each a sample when the field was at rest before it.
    type: {
      primary: "interaction.p50",
      metrics: ["interaction.p95", "run.duration", "frames.presented.count"],
      run: ({ page, target }) => page.type(target === "cocoa" ? "field" : "#field", text, { until: echo(target === "cocoa", text) }),
      reset: clear,
    },
    // What a focused field draws with no input: its caret.
    caret: {
      primary: "frames.presented.count",
      metrics: ["run.duration"],
      setup: ({ page, target }) => page.click(target === "cocoa" ? "field" : "#field"),
      run: ({ page }) => page.wait(3000),
    },
  },
})
