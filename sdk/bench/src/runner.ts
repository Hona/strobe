// The bench runner strobe spawns: imports a bench file and calls its functions on request (SPEC §9.6).
// Protocol: newline-delimited JSON on stdin/stdout. console.* goes to stderr.
import path from "node:path"
import { pathToFileURL } from "node:url"
import { createInterface } from "node:readline"

const write = (message: object) => process.stdout.write(JSON.stringify(message) + "\n")
for (const level of ["log", "info", "warn", "error", "debug"] as const)
  console[level] = (...parts: unknown[]) =>
    void process.stderr.write(parts.map((p) => (typeof p === "string" ? p : Bun.inspect(p))).join(" ") + "\n")

const file = path.resolve(process.argv[2] ?? "")
const spec = (await import(pathToFileURL(file).href)).default
if (!spec || typeof spec !== "object" || !spec.cases) throw new Error(`${file} must default-export bench({ targets, cases })`)

let seq = 0
const pending = new Map<string, { resolve: (v: unknown) => void; reject: (e: Error) => void }>()
const request = (op: string, args: object) =>
  new Promise<any>((resolve, reject) => {
    const id = `r${++seq}`
    pending.set(id, { resolve, reject })
    write({ id, op, args })
  })
const driver = (call: string, ...args: unknown[]) => request("driver", { call, args })

const page = {
  goto: (url: string, o = {}) => driver("goto", url, o),
  click: (selector: string, o = {}) => driver("click", selector, o),
  type: (selector: string, text: string, o = {}) => driver("type", selector, text, o),
  press: (key: string, o = {}) => driver("press", key, o),
  scroll: (selector: string, delta: object, o = {}) => driver("scroll", selector, delta, o),
  waitFor: (selector: string, o = {}) => driver("waitFor", selector, o),
  wait: (ms: number) => driver("wait", ms),
  eval: (fn: Function, ...args: unknown[]) => driver("eval", fn.toString(), ...args),
}

const state: { params: Record<string, unknown>; target: string; variant: string; playwright: any } = {
  params: {},
  target: "",
  variant: "base",
  playwright: null,
}

const context = () => ({
  page,
  params: state.params,
  target: state.target,
  variant: state.variant,
  async phase<T>(name: string, fn: () => T | Promise<T>) {
    await request("phase", { name, edge: "start" })
    try {
      return await fn()
    } finally {
      await request("phase", { name, edge: "end" })
    }
  },
})

function describe() {
  const targets = Object.fromEntries(
    Object.entries(spec.targets ?? {}).map(([name, t]: [string, any]) => [
      name,
      { kind: t.kind, url: t.url, href: t.href, id: t.id, path: t.path, home: t.home, viewport: t.viewport, scale: t.scale, prepare: !!t.prepare },
    ]),
  )
  const cases = Object.fromEntries(
    Object.entries(spec.cases).map(([name, c]: [string, any]) => [name, { primary: c.primary, metrics: c.metrics ?? [], reset: !!c.reset }]),
  )
  return { file, targets, params: spec.params ?? {}, cases, setup: !!spec.setup }
}

async function playwright(endpoint: string) {
  const from = path.dirname(file)
  const names = ["playwright", "@playwright/test", "playwright-core"]
  const resolved = names.map((n) => { try { return Bun.resolveSync(n, from) } catch { return undefined } }).find(Boolean)
  if (!resolved) throw new Error(`prepare needs Playwright: install playwright or @playwright/test next to ${file}`)
  const { chromium } = await import(resolved)
  const browser = await chromium.connectOverCDP(endpoint)
  const context = browser.contexts()[0]
  const page = context.pages()[0] ?? (await context.newPage())
  return { browser, context, page }
}

async function handle(op: string, args: any) {
  switch (op) {
    case "describe":
      return describe()
    case "block":
      Object.assign(state, { params: args.params ?? {}, target: args.target, variant: args.variant })
      return true
    case "prepare": {
      const target = spec.targets[state.target]
      if (!target?.prepare) return false
      state.playwright = await playwright(args.endpoint)
      await target.prepare({ params: state.params, target: state.target, variant: state.variant, playwright: state.playwright })
      return true
    }
    case "setup":
      await spec.setup?.(context())
      return true
    case "run":
      await spec.cases[args.case].run(context())
      return true
    case "reset": {
      const c = spec.cases[args.case]
      if (!c.reset) return false
      await c.reset(context())
      return true
    }
    case "close":
      if (state.playwright) await state.playwright.browser.close().catch(() => {})
      state.playwright = null
      return true
    case "exit":
      setTimeout(() => process.exit(0), 10)
      return true
  }
  throw new Error(`unknown op ${op}`)
}

createInterface({ input: process.stdin }).on("line", (line) => {
  if (!line.trim()) return
  const message = JSON.parse(line)
  const waiting = pending.get(message.id)
  if (waiting) {
    pending.delete(message.id)
    if ("error" in message) waiting.reject(new Error(message.error))
    else waiting.resolve(message.ok)
    return
  }
  handle(message.op, message.args ?? {}).then(
    (ok) => write({ id: message.id, ok: ok ?? null }),
    (error) => write({ id: message.id, error: String(error?.message ?? error), stack: String(error?.stack ?? "") }),
  )
})
write({ ready: true })
