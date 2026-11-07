// A fake native app for reduced motion (dev only, not shipped): no window; it says on stdout whether its
// launch carried the reduced-motion contract its profile declares (an argument, a variable and a settings
// key merged over its seeded settings, whose other keys must stay), then idles like a tray app.
import { readFileSync } from "fs"
import { join } from "path"

const home = process.env.HOME ?? process.env.USERPROFILE ?? "."
let settings: any = {}
try {
  settings = JSON.parse(readFileSync(join(home, "settings.json"), "utf8"))
} catch {}
const got = [process.argv.includes("--reduce-motion"), process.env.MOTION === "reduce", settings.motion === "reduce"]
setTimeout(() => console.log(got.every(Boolean) && settings.theme === "dark" ? "motion reduced, theme kept" : `motion not reduced: ${JSON.stringify({ got, settings })}`), 80)
setInterval(() => {}, 250)
