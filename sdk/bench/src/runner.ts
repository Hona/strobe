// The bench runner strobe spawns: imports a bench file and calls its functions on request (SPEC §9.6).
// Protocol: newline-delimited JSON over a loopback socket to strobe (STROBE_RUNNER_PORT). The runner's
// own stdio belongs to the bench file: stdin is empty, and stdout and stderr are the experiment's
// runner.log, so a process a hook spawns (Bun's `$` passes stdin on) never sees or blocks the protocol.
import path from "node:path"
import { connect } from "node:net"
import { pathToFileURL } from "node:url"
import { createInterface } from "node:readline"

const socket = connect(Number(process.env.STROBE_RUNNER_PORT), "127.0.0.1")
await new Promise((resolve, reject) => {
  socket.once("connect", resolve)
  socket.once("error", reject)
})
socket.setNoDelay(true)
const write = (message: object) => void socket.write(JSON.stringify(message) + "\n")

const file = path.resolve(process.argv[2] ?? "")
const spec = (await import(pathToFileURL(file).href)).default
if (!spec || typeof spec !== "object" || !spec.cases) throw new Error(`${file} must default-export bench({ targets, cases })`)
for (const [name, c] of Object.entries(spec.cases) as [string, any][])
  if (typeof c.run !== "function" && !c.launch) throw new Error(`case ${name} needs run(ctx) or launch: { until }`)

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
  click: (selector: unknown, o = {}) => driver("click", selector, o),
  hover: (selector: unknown, o = {}) => driver("hover", selector, o),
  move: (x: number, y: number, o = {}) => driver("move", x, y, o),
  type: (selector: unknown, text: string, o = {}) => driver("type", selector, text, o),
  press: (key: string, o = {}) => driver("press", key, o),
  down: (key: string, o = {}) => driver("down", key, o),
  // A path function is sampled every 4 ms of the drag's duration; strobe moves along the samples.
  drag: (selector: unknown, path: [number, number][] | ((t: number) => [number, number]), o: { duration?: number } = {}) => {
    if (typeof path !== "function") return driver("drag", selector, path, o)
    const n = Math.max(2, Math.ceil((o.duration ?? 1000) / 4) + 1)
    return driver("drag", selector, Array.from({ length: n }, (_, i) => path(i / (n - 1))), o)
  },
  up: (key: string, o = {}) => driver("up", key, o),
  snapshot: (name: string) => driver("snapshot", name),
  scroll: (selector: unknown, delta: object, o = {}) => driver("scroll", selector, delta, o),
  wheel: (selector: unknown, delta: object, o = {}) => driver("wheel", selector, delta, o),
  window: (action: string, o = {}) => driver("window", action, o),
  act: (selector: unknown, action: string, o = {}) => driver("act", selector, action, o),
  query: (selector: unknown, o = {}) => driver("query", selector, o),
  waitFor: (selector: unknown, o = {}) => driver("waitFor", selector, o),
  idle: (o = {}) => driver("idle", o),
  wait: (ms: number) => driver("wait", ms),
  eval: (fn: Function, ...args: unknown[]) => driver("eval", fn.toString(), ...args),
}

const state: {
  case: string
  params: Record<string, unknown>
  target: string
  variant: string
  url: string
  root: string
  home: string | null
  env: Record<string, string> | null
  exe: string | null
  playwright: any
  prepared: unknown
  mocks: { pattern: string; test: RegExp; handler: Function }[]
  session: any
} = { case: "", params: {}, target: "", variant: "base", url: "", root: "", home: null, env: null, exe: null, playwright: null, prepared: undefined, mocks: [], session: null }

const context = () => ({
  page,
  case: state.case,
  params: state.params,
  target: state.target,
  variant: state.variant,
  url: state.url,
  root: state.root,
  home: state.home,
  env: state.env,
  exe: state.exe,
  prepared: state.prepared,
  async phase<T>(name: string, fn: () => T | Promise<T>) {
    await request("phase", { name, edge: "start" })
    try {
      return await fn()
    } finally {
      await request("phase", { name, edge: "end" })
    }
  },
  count(name: string, n = 1) {
    void request("count", { name, n })
  },
  call: (name: string, ...args: unknown[]) => request("call", { name, args }),
  // The watch goes in at the call, before whatever the hook does next; the wait follows.
  async waitForLine(match: string | RegExp, opts: { in?: string; timeout?: number } = {}) {
    const pattern = match instanceof RegExp ? plain(match) : { match }
    const id = await request("line", { step: "watch", ...pattern, in: opts.in ?? "output" })
    return request("line", { step: "wait", id, timeout: opts.timeout ?? 30_000 })
  },
})

const prepareContext = () => ({
  case: state.case,
  params: state.params,
  target: state.target,
  variant: state.variant,
  url: state.url,
  root: state.root,
  playwright: state.playwright,
  mock,
})

// The bench file's own options, as JSON: functions become flags, RegExps their source.
function plain(value: any): any {
  if (value instanceof RegExp) return { regex: { source: value.source, flags: value.flags } }
  if (Array.isArray(value)) return value.map(plain)
  if (value && typeof value === "object")
    return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, typeof v === "function" ? true : plain(v)]))
  return value
}

function describe() {
  const targets = Object.fromEntries(
    Object.entries(spec.targets ?? {}).map(([name, t]: [string, any]) => [
      name,
      { ...plain(t), url: typeof t.url === "function" ? "/" : t.url, prepare: !!t.prepare || typeof t.url === "function", teardown: !!t.teardown },
    ]),
  )
  const cases = Object.fromEntries(
    Object.entries(spec.cases).map(([name, c]: [string, any]) => [
      name,
      {
        primary: c.primary ?? (c.launch ? "launch.ready" : undefined),
        metrics: c.metrics ?? [],
        reset: !!c.reset,
        targets: c.targets,
        timeout: c.timeout,
        noworse: c.noworse,
        gpuCache: c.gpuCache,
        expect: c.expect && plain(c.expect),
        launch: c.launch && {
          ...plain(c.launch),
          milestones: Object.fromEntries(
            Object.entries(c.launch.milestones ?? {}).map(([m, d]: [string, any]) => [
              m,
              { in: d.in ?? "output", ...(d.match instanceof RegExp ? plain(d.match) : { match: d.match }) },
            ]),
          ),
        },
      },
    ]),
  )
  return { file, targets, params: spec.params ?? {}, cases, setup: !!spec.setup, teardown: !!spec.teardown }
}

async function playwright(endpoint: string, target: string) {
  const from = path.dirname(file)
  const names = ["playwright", "@playwright/test", "playwright-core"]
  const resolved = names.map((n) => { try { return Bun.resolveSync(n, from) } catch { return undefined } }).find(Boolean)
  if (!resolved) throw new Error(`prepare needs Playwright: install playwright or @playwright/test next to ${file}`)
  // A bench path canonicalized on Windows starts with \\?\, which Bun's import() cannot load.
  const { chromium } = await import(resolved.replace(/^\\\\\?\\/, ""))
  const browser = await chromium.connectOverCDP(endpoint)
  const context = browser.contexts()[0]
  return { browser, context, page: await strobePage(context, target) }
}

// The page strobe drives, by CDP target id: Playwright may attach to pages late, and a page of
// Playwright's own would never see strobe's navigation.
async function strobePage(context: any, target: string) {
  const deadline = Date.now() + 10_000
  while (Date.now() < deadline) {
    for (const page of context.pages()) {
      const session = await context.newCDPSession(page)
      const info = await session.send("Target.getTargetInfo").finally(() => session.detach().catch(() => {}))
      if (info.targetInfo.targetId === target) return page
    }
    await Bun.sleep(50)
  }
  throw new Error(`Playwright did not attach to the page strobe opened (${target}) in 10 s`)
}

// Mocks through the Fetch domain on only the mocked patterns: unlike Playwright's routing, which turns
// the HTTP cache off for the page, every other request loads as it would for a person.
async function mock(pattern: string, handler: Function) {
  if (!state.playwright) throw new Error("mock needs the block's browser: call it in prepare")
  const test = new RegExp("^" + pattern.replace(/[.+^${}()|[\]\\]/g, "\\$&").replaceAll("?", ".").replaceAll("*", ".*") + "$")
  state.mocks.push({ pattern, test, handler })
  if (!state.session) {
    state.session = await state.playwright.context.newCDPSession(state.playwright.page)
    state.session.on("Fetch.requestPaused", (event: any) => void answer(state.session, event))
  }
  await state.session.send("Fetch.enable", { patterns: state.mocks.map((m) => ({ urlPattern: m.pattern, requestStage: "Request" })) })
}

async function answer(session: any, event: any) {
  const { request: r, requestId } = event
  const headers: Record<string, string> = r.headers ?? {}
  const body: string | undefined = r.postData ?? (r.postDataEntries ? r.postDataEntries.map((e: any) => Buffer.from(e.bytes ?? "", "base64").toString()).join("") : undefined)
  const req = { url: r.url, method: r.method, headers, body, json: () => JSON.parse(body ?? "null") }
  try {
    // The latest pattern answers first, and `undefined` passes to earlier ones, as Playwright's routes do.
    for (const m of state.mocks.toReversed()) {
      if (!m.test.test(r.url)) continue
      const res = await m.handler(req)
      if (res === undefined) continue
      const raw = res.body
      const json = raw !== undefined && typeof raw !== "string" && !(raw instanceof Uint8Array)
      const bytes = raw === undefined ? new Uint8Array() : raw instanceof Uint8Array ? raw : new TextEncoder().encode(json ? JSON.stringify(raw) : raw)
      const out = { ...(json ? { "content-type": "application/json" } : {}), ...res.headers }
      return await session.send("Fetch.fulfillRequest", {
        requestId,
        responseCode: res.status ?? 200,
        responseHeaders: Object.entries(out).map(([name, value]) => ({ name, value: String(value) })),
        body: Buffer.from(bytes).toString("base64"),
      })
    }
    await session.send("Fetch.continueRequest", { requestId })
  } catch (error) {
    console.error(`mock ${r.method} ${r.url}: ${error instanceof Error ? error.stack : error}`)
    await session.send("Fetch.failRequest", { requestId, errorReason: "Failed" }).catch(() => {})
  }
}

async function handle(op: string, args: any) {
  switch (op) {
    case "describe":
      return describe()
    case "block":
      Object.assign(state, {
        case: args.case ?? "",
        params: args.params ?? {},
        target: args.target,
        variant: args.variant,
        url: args.url ?? "",
        root: args.root ?? "",
        home: args.home ?? null,
        env: args.env ?? null,
        exe: args.exe ?? null,
        prepared: undefined,
        mocks: [],
        session: null,
      })
      return true
    case "before":
      await spec.cases[args.case].launch.before?.({
        case: state.case,
        params: state.params,
        target: state.target,
        variant: state.variant,
        root: state.root,
        home: state.home,
        env: state.env,
        exe: state.exe,
      })
      return true
    case "prepare": {
      const target = spec.targets[state.target]
      if (target?.prepare) {
        state.playwright = await playwright(args.endpoint, args.target)
        state.prepared = await target.prepare(prepareContext())
      }
      // A `url` function names the first page from what `prepare` returned, so the block loads one page.
      const url = typeof target?.url === "function" ? await target.url({ case: state.case, prepared: state.prepared, params: state.params }) : undefined
      return { url }
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
    case "close": {
      // Teardown hooks run while the browser is still open; one that throws does not keep the rest.
      const errors: unknown[] = []
      await Promise.resolve(spec.teardown?.(context())).catch((e) => errors.push(e))
      await Promise.resolve(spec.targets[state.target]?.teardown?.(prepareContext())).catch((e) => errors.push(e))
      if (state.session) await state.session.detach().catch(() => {})
      if (state.playwright) await state.playwright.browser.close().catch(() => {})
      Object.assign(state, { playwright: null, session: null, mocks: [] })
      if (errors.length) throw new AggregateError(errors, `teardown failed: ${errors.map((e) => (e instanceof Error ? e.message : String(e))).join("; ")}`)
      return true
    }
    case "exit":
      setTimeout(() => process.exit(0), 10)
      return true
  }
  throw new Error(`unknown op ${op}`)
}

// strobe going away ends the runner.
socket.on("close", () => process.exit(0))

createInterface({ input: socket }).on("line", (line) => {
  if (!line.trim()) return
  const message = JSON.parse(line)
  const waiting = pending.get(message.id)
  if (waiting) {
    pending.delete(message.id)
    // strobe's fix for a failed call (a missing permission, a selector to try) rides along as `hint`.
    if ("error" in message) waiting.reject(Object.assign(new Error(message.error), message.hint ? { hint: message.hint } : {}))
    else waiting.resolve(message.ok)
    return
  }
  handle(message.op, message.args ?? {}).then(
    (ok) => write({ id: message.id, ok: ok ?? null }),
    (error) =>
      write({
        id: message.id,
        error: String(error?.message ?? error),
        stack: String(error?.stack ?? ""),
        ...(typeof error?.hint === "string" ? { hint: error.hint } : {}),
      }),
  )
})
write({ ready: true })
