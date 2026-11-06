// Dev smoke bench for seeded workloads (not shipped): `walk` clicks buttons and types words that ctx.random()
// picks, so `--seed <n>` replays the same actions (strobe query <rec> actions) and another seed sends others;
// every run starts the stream over, so warmup still converges. `fixed` uses no randomness (plan.used_random
// stays false, and the ledger ignores --seed). `broken` fails after a few random clicks: its actions are kept
// beside the check's result (failed-<case>.actions.json.gz). `exec` runs commands ctx.random() picks on a
// cli() target, which keeps no recording: its passing check keeps them as exec-shell.actions.json.gz.
import { bench, cli, url } from "@hona/strobe"

const page = new URL("pages/fuzz.html", import.meta.url).href
const WORDS = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel"]
const clear = () => {
  for (const el of document.querySelectorAll("[data-clicked]")) el.removeAttribute("data-clicked")
  ;(document.getElementById("text") as HTMLInputElement).value = ""
}
const pick = (random: () => number, n: number) => Math.floor(random() * n)

export default bench({
  targets: { fuzz: url(page), shell: cli("bun --version") },
  cases: {
    exec: {
      targets: ["shell"],
      primary: "cpu.work",
      resetCheck: false,
      async run({ exec, random }) {
        for (let i = 0; i < 3; i++) await exec(random() < 0.5 ? "bun --version" : "bun --revision")
      },
    },
    walk: {
      targets: ["fuzz"],
      primary: "run.duration",
      async run({ page, random, phase }) {
        await phase("clicks", async () => {
          for (let i = 0; i < 6; i++) {
            const b = pick(random, 12)
            await page.click(`#b${b}`, { until: `#b${b}[data-clicked]` })
          }
        })
        await phase("typing", () => page.type("#text", Array.from({ length: 3 }, () => WORDS[pick(random, WORDS.length)]).join(" ")))
      },
      reset: ({ page }) => page.eval(clear),
    },
    fixed: {
      targets: ["fuzz"],
      primary: "run.duration",
      run: async ({ page }) => {
        await page.click("#b1", { until: "#b1[data-clicked]" })
        await page.click("#b2", { until: "#b2[data-clicked]" })
      },
      reset: ({ page }) => page.eval(clear),
    },
    broken: {
      targets: ["fuzz"],
      primary: "run.duration",
      async run({ page, random }) {
        for (let i = 0; i < 3; i++) await page.click(`#b${pick(random, 12)}`)
        await page.click("#nothing", { timeout: 500 })
      },
    },
  },
})
