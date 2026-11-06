// Dev bench for OS input to pages in every engine (not shipped): each kind of page input (type, press,
// a held button, hover, scroll, wheel, drag, a held key) in one page per engine. `--check` of two
// targets on different engines checks both from outside, with the OS's input, as their implementation
// comparison measures them (SPEC 9.10). Run from the repo root on an unlocked, headed session:
//   strobe bench scripts/dev/os-input.bench.ts --base chromium --head firefox --check
//   strobe bench scripts/dev/os-input.bench.ts --base chromium --head webkit --check   (macOS)
//   strobe bench scripts/dev/os-input.bench.ts --base chromium --head firefox --plan
// Safari takes no OS input in a WebDriver session: its cases are refused InputMismatch on macOS.
import { bench, url } from "@hona/strobe"

const rows = Array.from({ length: 60 }, (_, i) => `<div>row ${i}</div>`).join("")
const page = `<!doctype html><title>os input</title>
<style>body{font:14px system-ui;margin:8px}p{margin:4px 0}#hover{width:160px;height:28px;background:#eee}
#list{height:160px;overflow:auto;border:1px solid #999}#list div{height:32px}
#area{position:relative;height:90px;border:1px solid #ccc}#box{position:absolute;left:0;top:10px;width:60px;height:60px;background:#36c;touch-action:none}</style>
<input id="field" style="width:280px"><p id="echo">echo:</p><p id="keys">keys: 0</p>
<button id="btn">hold</button><p id="held">held: no</p>
<div id="hover">hover</div>
<div id="list">${rows}</div><p id="pos">top: 0</p>
<div id="area"><div id="box"></div></div><p id="moved">x: 0</p>
<script>
field.addEventListener("input", () => { echo.textContent = "echo:" + field.value })
window.n = 0
addEventListener("keydown", () => { keys.textContent = "keys: " + ++window.n })
btn.addEventListener("pointerdown", () => { held.textContent = "held: yes" })
btn.addEventListener("pointerup", () => { held.textContent = "held: no" })
document.getElementById("hover").addEventListener("pointerenter", (e) => { e.target.dataset.hovered = "" })
list.addEventListener("scroll", () => { pos.textContent = "top: " + (list.scrollTop > 0 ? "moved" : "0") })
let from = null
box.addEventListener("pointerdown", (e) => { from = e.clientX - box.offsetLeft; box.setPointerCapture(e.pointerId) })
box.addEventListener("pointermove", (e) => { if (from === null) return; box.style.left = e.clientX - from + "px"; moved.textContent = "x: " + (box.offsetLeft > 100 ? "far" : "near") })
box.addEventListener("pointerup", () => { from = null })
</script>`

const viewport = { width: 640, height: 720 }
const pages = ["chromium", "firefox", "webkit"]
const text = (selector: string, text: string) => ({ selector, text })
const reset = () => {
  const el = (id: string) => document.getElementById(id) as HTMLElement
  ;(el("field") as HTMLInputElement).value = ""
  el("echo").textContent = "echo:"
  el("field").blur()
  ;(window as any).n = 0
  el("keys").textContent = "keys: 0"
  delete el("hover").dataset.hovered
  el("list").scrollTop = 0
  el("pos").textContent = "top: 0"
  el("box").style.left = "0px"
  el("moved").textContent = "x: 0"
}
const on = (run: (ctx: any) => Promise<void>, metrics = ["interaction.p50", "cpu.work"]) => ({
  targets: pages,
  primary: "run.duration",
  metrics,
  run,
  reset: async ({ page }: any) => {
    await page.eval(reset)
    await page.move(4, 4)
  },
})

export default bench({
  targets: {
    chromium: url(`data:text/html,${encodeURIComponent(page)}`, { viewport, engine: "chromium" }),
    firefox: url(`data:text/html,${encodeURIComponent(page)}`, { viewport, engine: "firefox" }),
    webkit: url(`data:text/html,${encodeURIComponent(page)}`, { viewport, engine: "webkit" }),
  },
  cases: {
    type: on(({ page }) => page.type("#field", "hello", { until: text("#echo", "echo:hello") })),
    press: on(({ page }) => page.press("ArrowDown", { selector: "#field", until: text("#keys", "keys: 1") })),
    hold: on(async ({ page }) => {
      await page.down("Mouse", { at: "#btn", until: text("#held", "held: yes") })
      await page.up("Mouse", { until: text("#held", "held: no") })
    }),
    hover: on(({ page }) => page.hover("#hover", { until: "#hover[data-hovered]" })),
    scroll: on(({ page }) => page.scroll("#list", { by: 400 }, { until: text("#pos", "top: moved") }), ["scroll.latency.p50", "cpu.work"]),
    wheel: on(({ page }) => page.wheel("#list", { dy: 200 }, { until: text("#pos", "top: moved") }), ["scroll.latency.p50", "cpu.work"]),
    drag: on(({ page }) => page.drag("#box", [[0, 0], [200, 0]], { duration: 400, until: text("#moved", "x: far") }), ["input.rate", "cpu.work"]),
    // Keys go to what has focus: a click into the page first (untimed) gives the page focus.
    "held key": on(async ({ page, untimed }) => {
      await untimed(() => page.click("#area"))
      await page.down("a")
      await page.up("a", { until: text("#keys", "keys: 1") })
    }),
  },
})
