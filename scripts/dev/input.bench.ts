// Dev smoke bench for the input devices a page sees (not shipped): each run counts what the page matched, so
// `--check` prints them: count.hover (hover: hover), count.fine (pointer: fine), count.anyfine (any-pointer:
// fine), count.nohover (hover: none), count.coarse (any-pointer: coarse), count.touch navigator.maxTouchPoints,
// count.touchapi 'ontouchstart' in window. A desktop page target matches the first three and none of the rest,
// headless too, or strobe refuses the block (DeviceMismatch) before setup. Run with --check, --engine
// chromium|firefox|webkit, with and without --headless.
// `--filter "app input"` (Linux, a Chromium at /usr/bin/chromium): the same page in a browser a command starts,
// as an app's page: it keeps the machine's input, which the result's `input` records (`none` on a machine
// without a mouse, with a NoPointer warning).
import { bench, command, url } from "@hona/strobe"

const page = new URL("pages/input.html", import.meta.url).href
const seen = {
  hover: "(hover: hover)",
  fine: "(pointer: fine)",
  anyfine: "(any-pointer: fine)",
  nohover: "(hover: none)",
  coarse: "(any-pointer: coarse)",
}
let matched: string[] = []
const counts = {
  primary: "run.duration",
  metrics: [...Object.keys(seen), "touch", "touchapi"].map((k) => `count.${k}`),
  run({ count }: any) {
    for (const name of matched) count(name)
  },
}

export default bench({
  targets: {
    desktop: url(page),
    browser: command(`/usr/bin/chromium --headless=new --no-sandbox --remote-debugging-port={port} --user-data-dir={home}/chromium ${page}`),
  },
  async setup({ page }: any) {
    matched = []
    for (const [name, query] of Object.entries(seen)) {
      if (await page.eval((q: string) => matchMedia(q).matches, query)) matched.push(name)
    }
    const touch = await page.eval(() => navigator.maxTouchPoints)
    for (let i = 0; i < touch; i++) matched.push("touch")
    if (await page.eval(() => "ontouchstart" in window)) matched.push("touchapi")
  },
  cases: {
    input: { targets: ["desktop"], ...counts },
    "app input": { targets: ["browser"], ...counts },
  },
})
