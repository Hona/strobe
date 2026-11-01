// The process a `script()` target benchmarks (SPEC §9.6): it imports the variant's module and calls
// its exports when strobe asks. Plain JavaScript, so Bun and Node both run it as is.
// Protocol: newline-delimited JSON on stdin/stdout; console output goes to stderr.
import { createInterface } from "node:readline"
import { pathToFileURL } from "node:url"

const out = process.stdout.write.bind(process.stdout)
const write = (message) => out(JSON.stringify(message) + "\n")
for (const level of ["log", "info", "warn", "error", "debug"]) console[level] = (...parts) => process.stderr.write(parts.join(" ") + "\n")

const mod = await import(pathToFileURL(process.argv[2]).href)

createInterface({ input: process.stdin }).on("line", async (line) => {
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

write({ ready: true, pid: process.pid })
