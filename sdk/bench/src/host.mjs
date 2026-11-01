// The process a `script()` target benchmarks (SPEC §9.6): it imports the variant's module and calls
// its exports when strobe asks. Plain JavaScript, so Bun and Node both run it as is.
// Protocol: newline-delimited JSON over a loopback socket to strobe (STROBE_HOST_PORT). The process's
// own stdio is the module's: stdin is empty and stdout and stderr go to the experiment's script.log,
// so a child the module spawns (Bun's `$` passes stdin on) never sees or blocks the protocol.
import { connect } from "node:net"
import { createInterface } from "node:readline"
import { pathToFileURL } from "node:url"

const socket = connect(Number(process.env.STROBE_HOST_PORT), "127.0.0.1")
await new Promise((resolve, reject) => {
  socket.once("connect", resolve)
  socket.once("error", reject)
})
socket.setNoDelay(true)
const write = (message) => socket.write(JSON.stringify(message) + "\n")

const mod = await import(pathToFileURL(process.argv[2]).href)

createInterface({ input: socket }).on("line", async (line) => {
  if (!line.trim()) return
  const { id, op, name, args } = JSON.parse(line)
  // `js.heap.retained`: the heap after a full collection, between runs.
  if (op === "heap") {
    if (globalThis.Bun) Bun.gc(true)
    else globalThis.gc?.()
    write({ id, ok: process.memoryUsage().heapUsed })
    return
  }
  if (op === "exit") {
    write({ id, ok: true })
    // A normal exit, so --cpu-prof and --heap-prof write their profiles.
    setTimeout(() => process.exit(0), 10)
    return
  }
  try {
    const fn = name.split(".").reduce((o, k) => o?.[k], mod)
    if (typeof fn !== "function") throw new Error(`${name} is not a function exported by ${process.argv[2]}`)
    const value = await fn(...(args ?? []))
    write({ id, ok: value === undefined ? null : value })
  } catch (error) {
    write({ id, error: String(error?.stack ?? error) })
  }
})
// strobe going away ends the host.
socket.on("close", () => process.exit(0))

write({ ready: true, pid: process.pid })
