// Dev smoke bench for pages in every engine (not shipped): a cold first visit timed to a selector, and
// input aimed at elements the viewport shows only part of. Run with --engine chromium|firefox|webkit.
import { bench, url } from "@hona/strobe"

const page = (name: string) => new URL(`pages/${name}.html`, import.meta.url).href
const unclick = () => {
  for (const el of document.querySelectorAll("[data-clicked]")) el.removeAttribute("data-clicked")
  scrollTo({ top: 0, left: 0, behavior: "instant" })
}

export default bench({
  targets: {
    launch: url(page("launch"), {
      // Before the page's own scripts in the launch's document: its mark is a milestone.
      prepare: ({ addInitScript }) => addInitScript(() => performance.mark("init")),
    }),
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
    // The composer's centre is 1500 px below a 900 px window: a click aims at the part that shows.
    "tall composer": {
      targets: ["click"],
      primary: "run.duration",
      run: ({ page }) => page.click("#composer", { until: "#composer[data-clicked]" }),
      reset: ({ page }) => page.eval(unclick),
    },
    "below the fold": {
      targets: ["click"],
      primary: "run.duration",
      run: ({ page }) => page.click("#below", { until: "#below[data-clicked]" }),
      reset: ({ page }) => page.eval(unclick),
    },
    "half off the edge": {
      targets: ["click"],
      primary: "run.duration",
      run: ({ page }) => page.click("#wide", { until: "#wide[data-clicked]" }),
      reset: ({ page }) => page.eval(unclick),
    },
    // Nothing of it shows even scrolled into view: refused OutOfView.
    clipped: {
      targets: ["click"],
      primary: "run.duration",
      run: ({ page }) => page.click("#clipped", { until: "#clipped[data-clicked]" }),
      reset: ({ page }) => page.eval(unclick),
    },
  },
})
