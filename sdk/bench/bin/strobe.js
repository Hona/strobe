#!/usr/bin/env node
// Runs this platform's strobe binary, from the @hona/strobe-<platform>-<arch> package npm installed beside this
// one (an optional dependency: each machine installs only its own). STROBE_BINARY points at another build.
import { spawnSync } from "node:child_process"
import { createRequire } from "node:module"
import { dirname, join } from "node:path"

const target = `${process.platform}-${process.arch}`
let exe = process.env.STROBE_BINARY
if (!exe) {
  try {
    const require = createRequire(import.meta.url)
    const dir = dirname(require.resolve(`@hona/strobe-${target}/package.json`))
    exe = join(dir, process.platform === "win32" ? "strobe.exe" : "strobe")
  } catch {
    console.error(`strobe: no build for ${target} is installed (@hona/strobe-${target}). strobe ships for win32-x64 so far.`)
    process.exit(1)
  }
}
const r = spawnSync(exe, process.argv.slice(2), { stdio: "inherit" })
if (r.error) {
  console.error(`strobe: ${r.error.message}`)
  process.exit(1)
}
if (r.signal) process.kill(process.pid, r.signal)
process.exit(r.status ?? 1)
