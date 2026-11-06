// Dev smoke bench for pages in every engine (not shipped): cold first visits timed to a selector (one in a
// declarative shadow root), and input aimed at elements the viewport shows only part of, or something covers.
// Run with --engine chromium|firefox|webkit. `clipped` refuses OutOfView and `covered` refuses Covered.
import { bench, url } from "@hona/strobe"

const page = (name: string) => new URL(`pages/${name}.html`, import.meta.url).href
const unclick = () => {
  for (const el of document.querySelectorAll("[data-clicked]")) el.removeAttribute("data-clicked")
  scrollTo({ top: 0, left: 0, behavior: "instant" })
}
const click = (id: string) => ({
  targets: ["click"],
  primary: "run.duration",
  run: ({ page }: any) => page.click(`#${id}`, { until: `#${id}[data-clicked]` }),
  reset: ({ page }: any) => page.eval(unclick),
})

export default bench({
  targets: {
    launch: url(page("launch"), {
      // Before the page's own scripts in the launch's document: its mark is a milestone.
      prepare: ({ addInitScript }) => addInitScript(() => performance.mark("init")),
    }),
    shadow: url(page("launch-dsd")),
    click: url(page("click")),
  },
  cases: {
    "first visit": {
      targets: ["launch"],
      primary: "launch.ready",
      metrics: ["cpu.work", "launch.init", "launch.booted", "launch.shown", "launch.painted"],
      launch: {
        until: "#ready",
        milestones: { init: { match: "init", in: "page" }, booted: { match: "booted", in: "page" }, shown: { match: "shown", in: "page" } },
      },
    },
    // The until element appears only inside a root the parser attached.
    "first visit in a shadow root": {
      targets: ["shadow"],
      primary: "launch.ready",
      metrics: ["launch.shown"],
      launch: { until: "#app >>> #ready", milestones: { shown: { match: "shown", in: "page" } }, timeout: 10_000 },
    },
    // The composer's centre is 1500 px below a 900 px window: a click aims at the part that shows.
    "tall composer": click("composer"),
    "below the fold": click("below"),
    "half off the edge": click("wide"),
    // Nothing of it shows even scrolled into view: refused OutOfView.
    clipped: click("clipped"),
    // An overlay over all of it: refused Covered.
    covered: click("covered"),
    // A badge over its centre: aimed at a quarter that shows.
    badged: click("badged"),
    "wrapped link": click("wrapped"),
    // Taller than the shadow root's scroller it is slotted into.
    slotted: click("slotted"),
    // In a clipper scaled to half.
    scaled: click("scaled"),
  },
})
