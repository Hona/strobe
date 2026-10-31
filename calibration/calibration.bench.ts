// strobe's own calibration bench: known costs that every probe must recover (SPEC §10.3).
import { bench, url } from "@strobe/bench"

const page = (query: string) => `http://127.0.0.1:${process.env.STROBE_CALIBRATION_PORT ?? 4821}/?${query}`

export default bench({
  targets: {
    page: url(page("case=scroll")),
  },
  cases: {
    scroll: {
      primary: "frames.interval.p95",
      async run({ page, phase }) {
        await phase("down", () => page.scroll("#list", { by: 3000, speed: 3000 }))
        await phase("up", () => page.scroll("#list", { by: -3000, speed: 3000 }))
      },
    },
    busy: {
      primary: "frames.dropped.pct",
      run: ({ page }) => page.wait(1000),
    },
  },
})
