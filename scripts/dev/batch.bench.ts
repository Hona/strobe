// Dev bench for many cases per launch (not shipped): 40 small cases on one page, most of them clean (the file's
// `reset` puts the page back), and some with reset bugs on purpose: `dirty-type` and `dirty-check` leave a
// control changed (ResetIncomplete, caught after their own runs, before the next case), `heading-left` leaves
// another heading that the next case's `expect` does not show (that case runs in a fresh launch), `crash` takes
// the page's renderer down, `throws` fails its run, and `builds-on` builds on its own runs (resetCheck: false)
// and so ends its launch. Run from the repo root and compare the wall time with and without --batch:
//   strobe bench scripts/dev/batch.bench.ts --head . --check --headless [--batch 10]
import { bench, url } from "@hona/strobe"

const page = new URL("pages/batch.html", import.meta.url).href

const restore = () => {
  ;(document.getElementById("text") as HTMLInputElement).value = ""
  ;(document.getElementById("check") as HTMLInputElement).checked = false
  document.getElementById("count")!.textContent = "0"
  document.getElementById("list")!.replaceChildren()
  document.getElementById("title")!.textContent = "Batch"
}

type Case = Parameters<typeof bench>[0]["cases"][string]

const clean = (i: number): [string, Case] => {
  switch (i % 3) {
    case 0:
      return [
        `type-${i}`,
        {
          primary: "cpu.work",
          metrics: ["main.work"],
          async run({ page }) {
            await page.type("#text", `case ${i}`, { burst: true })
          },
        },
      ]
    case 1:
      return [
        `check-${i}`,
        {
          primary: "cpu.work",
          async run({ page }) {
            await page.click("#check")
          },
        },
      ]
    default:
      return [
        `add-${i}`,
        {
          primary: "cpu.work",
          metrics: ["count.adds"],
          async run({ page, count }) {
            for (let k = 0; k < 3; k++) {
              await page.click("#add")
              count("adds")
            }
          },
        },
      ]
  }
}

const cases: Record<string, Case> = {}
for (let i = 0; i < 33; i++) {
  const [name, c] = clean(i)
  cases[name] = c
  if (i === 4)
    cases["dirty-type"] = {
      primary: "cpu.work",
      async run({ page }) {
        await page.type("#text", "left over", { burst: true })
      },
      // The bug: nothing is taken back.
      async reset() {},
    }
  if (i === 10)
    cases["dirty-check"] = {
      primary: "cpu.work",
      async run({ page }) {
        await page.click("#check")
      },
      async reset() {},
    }
  if (i === 15) {
    cases["heading-left"] = {
      primary: "cpu.work",
      async run({ page }) {
        await page.eval(() => (document.getElementById("title")!.textContent = "Other"))
      },
      async reset() {},
    }
    cases["heading-next"] = {
      primary: "cpu.work",
      expect: { heading: "Batch" },
      async run({ page }) {
        await page.click("#add")
      },
    }
  }
  if (i === 20)
    cases.crash = {
      primary: "cpu.work",
      timeout: 5000,
      async run({ page }) {
        await page.goto("chrome://crash")
      },
    }
  if (i === 24)
    cases.throws = {
      primary: "cpu.work",
      async run() {
        throw new Error("this case fails on purpose")
      },
    }
  if (i === 28)
    // Each run adds to the last one's text on purpose.
    cases["builds-on"] = {
      primary: "cpu.work",
      resetCheck: false,
      async run({ page }) {
        await page.type("#text", "more ", { burst: true })
      },
      async reset() {},
    }
}

export default bench({
  targets: { page: url(page) },
  async setup({ page }) {
    await page.waitFor("#text")
  },
  // Every case without a reset of its own puts the whole page back.
  reset: ({ page }) => page.eval(restore),
  cases,
})
