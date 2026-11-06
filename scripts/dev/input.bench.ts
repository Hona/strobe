// Dev smoke bench for the input devices a page sees (not shipped): each run counts what the page matched, so
// `--check` prints them: count.hover (hover: hover), count.fine (pointer: fine), count.anyfine (any-pointer:
// fine), count.nohover (hover: none), count.coarse (pointer: coarse), count.touch navigator.maxTouchPoints.
// A desktop page target matches the first three with no touch points, headless too, or strobe refuses the
// block (DeviceMismatch) before setup. Run with --check, --engine chromium|firefox|webkit, with and without
// --headless.
import { bench, url } from "@hona/strobe"

const seen = {
  hover: "(hover: hover)",
  fine: "(pointer: fine)",
  anyfine: "(any-pointer: fine)",
  nohover: "(hover: none)",
  coarse: "(pointer: coarse)",
}
let matched: string[] = []

export default bench({
  targets: { desktop: url(new URL("pages/input.html", import.meta.url).href) },
  async setup({ page }: any) {
    matched = []
    for (const [name, query] of Object.entries(seen)) {
      if (await page.eval((q: string) => matchMedia(q).matches, query)) matched.push(name)
    }
    const touch = await page.eval(() => navigator.maxTouchPoints)
    for (let i = 0; i < touch; i++) matched.push("touch")
  },
  cases: {
    input: {
      primary: "run.duration",
      metrics: [...Object.keys(seen), "touch"].map((k) => `count.${k}`),
      run({ count }: any) {
        for (const name of matched) count(name)
      },
    },
  },
})
