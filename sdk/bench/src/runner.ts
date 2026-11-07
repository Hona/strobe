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
  if (typeof c.run !== "function" && !(c.run && typeof c.run === "object") && !c.launch) throw new Error(`case ${name} needs run(ctx), run: { <target>: (ctx) => … }, or launch: { until }`)

// A target's adapter (`ui`): `@name` in a selector is its entry there, and `ctx.ui.<name>` its
// selector or its step bound to the hook's context (SPEC §9.10).
const ui = (target: string): Record<string, any> => spec.targets?.[target]?.ui ?? {}
const names = new Set(Object.values(spec.targets ?? {}).flatMap((t: any) => Object.keys(t?.ui ?? {})))

class ParityGap extends Error {
  refusal: object
  constructor(target: string, what: string) {
    const summary = `${spec.targets?.[target]?.name ?? target} has no ${what}`
    super(`ParityGap: ${summary}`)
    this.refusal = { validator: "ParityGap", summary, fix: `give ${target}'s ui ${what}, or mark the case unsupported there` }
  }
}

// `@name` (optionally followed by filters, `@row index=-1`) as the running side's selector.
function alias(sel: unknown): unknown {
  if (sel && typeof sel === "object" && !Array.isArray(sel) && typeof (sel as any).selector === "string")
    return { ...(sel as object), selector: alias((sel as any).selector) }
  if (typeof sel !== "string" || !sel.startsWith("@")) return sel
  const [, name, rest] = sel.match(/^@([A-Za-z_$][\w$]*)\s*(.*)$/s) ?? []
  if (!name) return sel
  const entry = ui(state.target)[name]
  if (entry === undefined && !names.has(name)) throw new Error(`@${name} is in no target's ui (${[...names].map((n) => "@" + n).join(", ") || "none has one"}): a typo?`)
  if (entry === undefined) throw new ParityGap(state.target, `@${name}`)
  if (typeof entry !== "string") throw new Error(`@${name} is a step of ${state.target}'s ui, not a selector: call ctx.ui.${name}()`)
  return rest ? `${entry} ${rest}` : entry
}

// A call's options with their selectors (`until`, `selector`, `at`) resolved.
const opts = (o: any) => {
  if (!o || typeof o !== "object") return o
  const out = { ...o }
  for (const k of ["until", "selector", "at"]) if (k in out && !Array.isArray(out[k])) out[k] = alias(out[k])
  return out
}

// A function's source without its comments (a commented-out step names nothing), strings kept.
function code(source: string): string {
  let out = ""
  for (let i = 0; i < source.length; i++) {
    const c = source[i]
    if (c === '"' || c === "'" || c === "`") {
      let j = i + 1
      while (j < source.length && source[j] !== c) j += source[j] === "\\" ? 2 : 1
      out += source.slice(i, j + 1)
      i = j
    } else if (c === "/" && source[i + 1] === "/") {
      while (i < source.length && source[i] !== "\n") i++
      out += "\n"
    } else if (c === "/" && source[i + 1] === "*") {
      const end = source.indexOf("*/", i + 2)
      i = end < 0 ? source.length : end + 1
    } else out += c
  }
  return out
}

// `@name`s in a selector's place (an action's first argument, `until`, `selector`, `at`) that no
// target's ui has: a typo, said before anything runs. Elsewhere `@word` may be typed text.
function typos(source: string): string[] {
  const out = new Set<string>()
  const at = /(?:\.(?:click|hover|type|scroll|wheel|drag|act|query|waitFor)\(\s*|\b(?:until|selector|at)\s*:\s*)["'`]@([A-Za-z_$][\w$]*)/g
  for (const m of code(source).matchAll(at)) if (!names.has(m[1])) out.add(`@${m[1]}`)
  return [...out]
}

// The `@name`s and `ui.<name>`s a function's source names (only names some target's ui has: typed
// text such as "@mention" is no adapter entry).
function named(raw: string): string[] {
  const source = code(raw)
  const out = new Set<string>()
  for (const m of source.matchAll(/["'`]@([A-Za-z_$][\w$]*)/g)) if (names.has(m[1])) out.add(m[1])
  for (const m of source.matchAll(/\bui\s*(?:\.\s*([A-Za-z_$][\w$]*)|\[\s*["'`]([A-Za-z_$][\w$]*)["'`]\s*\])/g)) if (names.has(m[1] ?? m[2])) out.add(m[1] ?? m[2])
  return [...out]
}

// A hook of a case on a target: one function, or the target's entry of a per-target map.
const hookFor = (h: any, target: string) => (typeof h === "function" ? h : h && typeof h === "object" ? h[target] : undefined)

// What a target lacks to run a case, found before anything runs (a journey is never half run): the
// adapter entries its hooks, `expect` and `until` name, through the steps they call, or its own run.
function gapOf(c: any, target: string): { missing: string; reason: string } | undefined {
  if (c.run && typeof c.run === "object" && typeof c.run[target] !== "function") return { missing: "run", reason: `the case gives no run for ${target}` }
  // A launch case runs no setup; its `before` hook runs before the launch.
  const sources = [c.launch ? c.launch.before : spec.setup, hookFor(c.run, target), c.launch ? undefined : hookFor(c.reset ?? spec.reset, target)].filter((f) => typeof f === "function").map(String)
  sources.push(JSON.stringify(plain(c.expect ?? null)), JSON.stringify(plain(c.launch?.until ?? null)))
  const own = ui(target)
  const seen = new Set<string>()
  const todo = sources.flatMap(named)
  while (todo.length) {
    const n = todo.pop()!
    if (seen.has(n)) continue
    seen.add(n)
    if (own[n] === undefined) return { missing: `@${n}`, reason: `no @${n} in ${target}'s ui` }
    if (typeof own[n] === "function") todo.push(...named(String(own[n])))
  }
}

let seq = 0
const pending = new Map<string, { resolve: (v: unknown) => void; reject: (e: Error) => void }>()
const request = (op: string, args: object) =>
  new Promise<any>((resolve, reject) => {
    const id = `r${++seq}`
    pending.set(id, { resolve, reject })
    write({ id, op, args })
  })
const driver = (call: string, ...args: unknown[]) => request("driver", { call, args })

// Every selector, and every option that holds one, goes through the side's adapter before strobe sees it.
const page = {
  goto: (url: string, o = {}) => driver("goto", url, opts(o)),
  click: (selector: unknown, o = {}) => driver("click", alias(selector), opts(o)),
  hover: (selector: unknown, o = {}) => driver("hover", alias(selector), opts(o)),
  move: (x: number, y: number, o = {}) => driver("move", x, y, opts(o)),
  type: (selector: unknown, text: string, o = {}) => driver("type", alias(selector), text, opts(o)),
  press: (key: string, o = {}) => driver("press", key, opts(o)),
  down: (key: string, o = {}) => driver("down", key, opts(o)),
  // A path function is sampled every 4 ms of the drag's duration; strobe moves along the samples.
  drag: (selector: unknown, path: [number, number][] | ((t: number) => [number, number]), o: { duration?: number } = {}) => {
    if (typeof path !== "function") return driver("drag", alias(selector), path, opts(o))
    const n = Math.max(2, Math.ceil((o.duration ?? 1000) / 4) + 1)
    return driver("drag", alias(selector), Array.from({ length: n }, (_, i) => path(i / (n - 1))), opts(o))
  },
  up: (key: string, o = {}) => driver("up", key, opts(o)),
  snapshot: (name: string) => driver("snapshot", name),
  scroll: (selector: unknown, delta: object, o = {}) => driver("scroll", alias(selector), delta, opts(o)),
  wheel: (selector: unknown, delta: object, o = {}) => driver("wheel", alias(selector), delta, opts(o)),
  window: (action: unknown, o = {}) => driver("window", action, opts(o)),
  act: (selector: unknown, action: string, o = {}) => driver("act", alias(selector), action, opts(o)),
  query: (selector: unknown, o = {}) => driver("query", alias(selector), opts(o)),
  waitFor: (selector: unknown, o = {}) => driver("waitFor", alias(selector), opts(o)),
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
  fixture: { url: string; port: number; pid: number; home: string } | null
  engine: string
  relay: any
  devtools: { endpoint: string; target: string } | null
  /** The experiment's seed (`--seed`, or drawn by strobe), the block's case seed, and the hook's stream. */
  seed: number
  caseSeed: number
  stream: () => number
  /** A hook read `ctx.seed` or called `ctx.random()`: the workload depends on the seed (the ledger's rule). */
  usedRandom: boolean
} = { case: "", params: {}, target: "", variant: "base", url: "", root: "", home: null, env: null, exe: null, playwright: null, prepared: undefined, mocks: [], session: null, fixture: null, engine: "chromium", relay: null, devtools: null, seed: 0, caseSeed: 0, stream: () => 0, usedRandom: false }

// Seeds (SPEC §9.6): a case's seed is mix(experiment seed, case + "\n" + its params as canonical JSON), so
// base and head, and every target of a journey, get the same one. Each hook call starts its own stream
// from mix(case seed, hook): every run starts from the same point, so warmup converges and the runs repeat.
// mix: FNV-1a over the text's UTF-8 bytes from `seed`, then murmur3's finalizer; the stream is mulberry32.
function mix(seed: number, text: string): number {
  let h = (0x811c9dc5 ^ seed) >>> 0
  for (const b of new TextEncoder().encode(text)) h = Math.imul(h ^ b, 0x01000193)
  h ^= h >>> 16
  h = Math.imul(h, 0x85ebca6b)
  h ^= h >>> 13
  h = Math.imul(h, 0xc2b2ae35)
  h ^= h >>> 16
  return h >>> 0
}

function mulberry32(a: number): () => number {
  return () => {
    a = (a + 0x6d2b79f5) | 0
    let t = Math.imul(a ^ (a >>> 15), 1 | a)
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
}

// JSON with object keys sorted at every level: the same params give the same text in any order.
function canonical(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`
  if (value && typeof value === "object")
    return `{${Object.keys(value).sort().map((k) => `${JSON.stringify(k)}:${canonical((value as any)[k])}`).join(",")}}`
  return JSON.stringify(value) ?? "null"
}

// The hook about to run starts its stream (`ctx.random()`) over.
const stream = (hook: string) => void (state.stream = mulberry32(mix(state.caseSeed, hook)))

// A batch (`--check --batch`) runs several cases in one launch: a run or reset of another case than the
// last one's switches the hooks' case and seed to it.
function forCase(name: string) {
  if (!name || name === state.case) return
  state.case = name
  state.caseSeed = mix(state.seed, `${name}\n${canonical(state.params)}`)
}

// A launch hook (prepare, a `url` function, setup) runs once for every case of a batch's launch: reading
// `ctx.case` there is said (`read_case`), and strobe refuses the batch.
let launchHook = false
let readCase = false
const caseOf = () => {
  if (launchHook) readCase = true
  return state.case
}

// `ctx.seed` and `ctx.random()` on a hook's context. A getter, so only reading it counts as using it
// (never copy a context with spread syntax: that reads every getter).
const seeded = <T extends object>(ctx: T): T =>
  Object.defineProperties(ctx, {
    seed: {
      enumerable: true,
      get() {
        state.usedRandom = true
        return state.caseSeed
      },
    },
    random: {
      enumerable: true,
      value() {
        state.usedRandom = true
        return state.stream()
      },
    },
  })

// `ctx.ui.<name>`: the side's selector, or its step called with this context; a name it lacks is a parity gap.
const uiOf = (ctx: () => object) =>
  new Proxy({}, {
    get(_, name) {
      if (typeof name !== "string") return undefined
      const entry = ui(state.target)[name]
      if (entry === undefined) throw new ParityGap(state.target, `@${name}`)
      return typeof entry === "function" ? (...args: unknown[]) => entry(ctx(), ...args) : entry
    },
    has: (_, name) => typeof name === "string" && name in ui(state.target),
  })

const context = (): any => seeded({
  page,
  ui: uiOf(context),
  get case() {
    return caseOf()
  },
  params: state.params,
  target: state.target,
  variant: state.variant,
  url: state.url,
  root: state.root,
  home: state.home,
  env: state.env,
  exe: state.exe,
  prepared: state.prepared,
  fixture: state.fixture,
  async phase<T>(name: string, fn: () => T | Promise<T>) {
    await request("phase", { name, edge: "start" })
    try {
      return await fn()
    } finally {
      await request("phase", { name, edge: "end" })
    }
  },
  // Each edge waits for strobe's readings of the target, so the stretch starts after them and ends before them.
  async untimed<T>(fn: () => T | Promise<T>) {
    await request("untimed", { edge: "start" })
    try {
      return await fn()
    } finally {
      await request("untimed", { edge: "end" })
    }
  },
  count(name: string, n = 1) {
    void request("count", { name, n })
  },
  call: (name: string, ...args: unknown[]) => request("call", { name, args }),
  exec: (line?: string) => request("exec", { line }),
  // The watch goes in at the call, before whatever the hook does next; then short polls, each answered
  // at once, so strobe serves the hook's other calls (a click) between them.
  async waitForLine(match: string | RegExp, opts: { in?: string; timeout?: number } = {}) {
    const pattern = match instanceof RegExp ? plain(match) : { match }
    const id = await request("line", { step: "watch", ...pattern, in: opts.in ?? "output", timeout: opts.timeout ?? 30_000 })
    for (;;) {
      const got: any = await request("line", { step: "poll", id })
      if (!got?.pending) return got
      await Bun.sleep(10)
    }
  },
}) as any

// A script every new document of the block's page runs before its own, in any engine: strobe adds it
// through its own connection (CDP's Page.addScriptToEvaluateOnNewDocument, BiDi's script.addPreloadScript).
// A function is sent as its source, called with `arg` (JSON), as Playwright's page.addInitScript does.
function addInitScript(script: string | Function, arg?: unknown) {
  if (typeof script === "string" && arg !== undefined) throw new Error("addInitScript: a string script takes no argument; pass a function")
  // A string runs as a function's body in every engine (Firefox's preload scripts are functions), so its
  // top-level declarations stay its own wherever it runs.
  const source = typeof script === "function" ? `(${script.toString()})(${arg === undefined ? "" : JSON.stringify(arg)})` : `(() => {\n${script}\n})()`
  return request("init", { source }).then(() => undefined)
}

// Playwright connects over CDP, which only Chromium speaks: on another engine reading it throws, typed
// as strobe's EngineUnsupported refusal, so the bench names the cause instead of a null page failing later.
const prepareContext = () => seeded({
  get case() {
    return caseOf()
  },
  params: state.params,
  target: state.target,
  variant: state.variant,
  url: state.url,
  root: state.root,
  get playwright() {
    if (state.engine === "chromium") {
      if (!state.playwright) throw new Error(`ctx.playwright needs Playwright: install playwright or @playwright/test next to ${file}`)
      return state.playwright
    }
    const summary = `ctx.playwright connects over Chromium's DevTools protocol; this target runs in ${state.engine}`
    const fix =
      state.engine === "webkit"
        ? "use ctx.addInitScript (WebKit runs it after the first document's own scripts), the page driver, serve mocked responses from a local server (WebKit has no ctx.mock), or run the target in Chromium"
        : "use ctx.addInitScript for scripts before the page's own, ctx.mock for network mocks and the page driver for the rest (they work in Chromium and Firefox), or run the target in Chromium with Playwright installed"
    throw Object.assign(new Error(`EngineUnsupported: ${summary}`), { hint: fix, refusal: { validator: "EngineUnsupported", summary, fix } })
  },
  mock,
  addInitScript,
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
      // `ui`: selectors as strings, steps as `{ step: true }` (strobe resolves `@name` in `expect` and `until`).
      {
        ...plain(t),
        url: typeof t.url === "function" ? "/" : t.url,
        prepare: !!t.prepare || typeof t.url === "function",
        teardown: !!t.teardown,
        ui: Object.fromEntries(Object.entries(t.ui ?? {}).map(([k, v]) => [k, typeof v === "function" ? { step: true } : v])),
      },
    ]),
  )
  const cases = Object.fromEntries(
    Object.entries(spec.cases).map(([name, c]: [string, any]) => [
      name,
      {
        primary: c.primary ?? (c.launch ? "launch.ready" : undefined),
        metrics: c.metrics ?? [],
        // A run case without a reset of its own takes the file's (`bench({ reset })`).
        reset: !c.launch && !!(c.reset ?? spec.reset),
        adapted: c.run && typeof c.run === "object" ? "steps" : null,
        gaps: Object.fromEntries(Object.keys(spec.targets ?? {}).flatMap((t) => { const g = gapOf(c, t); return g ? [[t, g]] : [] })),
        typos: [...new Set([spec.setup, c.launch?.before, c.run, c.reset ?? (c.launch ? undefined : spec.reset)].flatMap((h) => (typeof h === "function" ? [h] : h && typeof h === "object" ? Object.values(h) : [])).flatMap((f: any) => typos(String(f))))],
        resetCheck: c.resetCheck !== false,
        targets: c.targets,
        timeout: c.timeout,
        noworse: c.noworse,
        gpuCache: c.gpuCache,
        hero: c.hero === true,
        unsupported: c.unsupported,
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
  return { file, targets, params: spec.params ?? {}, cases, setup: !!spec.setup, teardown: !!spec.teardown, fixture: spec.fixture && plain(spec.fixture) }
}

// Playwright, when it is installed next to the bench file: a prepare that uses only ctx.addInitScript and
// ctx.mock needs none on another engine, and none on Chromium but for ctx.mock.
async function playwright(endpoint: string, target: string) {
  const from = path.dirname(file)
  const names = ["playwright", "@playwright/test", "playwright-core"]
  const resolved = names.map((n) => { try { return Bun.resolveSync(n, from) } catch { return undefined } }).find(Boolean)
  if (!resolved) return null
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
  const test = new RegExp("^" + pattern.replace(/[.+^${}()|[\]\\]/g, "\\$&").replaceAll("?", ".").replaceAll("*", ".*") + "$")
  if (state.engine !== "chromium") {
    // Another engine has one remote connection, strobe's: strobe intercepts and asks these handlers.
    state.mocks.push({ pattern, test, handler })
    const where = await request("mock", { pattern })
    if (!state.relay) state.relay = await relay(where.port, where.token)
    return
  }
  if (!state.devtools) throw new Error("mock needs the block's browser: call it in prepare")
  state.mocks.push({ pattern, test, handler })
  if (!state.session) {
    state.session = await cdpSession(state.devtools.endpoint, state.devtools.target)
    state.session.on("Fetch.requestPaused", (event: any) => void answer(state.session, event))
  }
  await state.session.send("Fetch.enable", { patterns: state.mocks.map((m) => ({ urlPattern: m.pattern, requestStage: "Request" })) })
}

// A DevTools session of the runner's own on the page strobe drives (by its target id), with no Playwright:
// `send` and `on` for its events, as Playwright's CDPSession has them.
async function cdpSession(endpoint: string, target: string) {
  const ws = new WebSocket(endpoint)
  await new Promise((resolve, reject) => {
    ws.onopen = resolve
    ws.onerror = () => reject(new Error(`mock: the browser's DevTools endpoint ${endpoint} did not answer`))
  })
  let seq = 0
  let sessionId: string | undefined
  const waiting = new Map<number, { resolve: (v: any) => void; reject: (e: Error) => void }>()
  const listeners = new Map<string, ((params: any) => void)[]>()
  ws.onmessage = (e) => {
    const m = JSON.parse(String(e.data))
    const w = m.id === undefined ? undefined : waiting.get(m.id)
    if (w) {
      waiting.delete(m.id)
      return m.error ? w.reject(new Error(`cdp: ${m.error.message}`)) : w.resolve(m.result)
    }
    if (m.method && m.sessionId === sessionId) for (const f of listeners.get(m.method) ?? []) f(m.params)
  }
  // The browser went away (the block closed it): no reply will come to what still waits.
  ws.onclose = () => {
    for (const w of waiting.values()) w.reject(new Error("cdp: the DevTools connection closed"))
    waiting.clear()
  }
  const send = (method: string, params: object = {}, session = sessionId) =>
    new Promise<any>((resolve, reject) => {
      const id = ++seq
      waiting.set(id, { resolve, reject })
      ws.send(JSON.stringify({ id, method, params, ...(session ? { sessionId: session } : {}) }))
    })
  sessionId = (await send("Target.attachToTarget", { targetId: target, flatten: true })).sessionId
  return {
    send,
    on: (method: string, f: (params: any) => void) => void listeners.set(method, [...(listeners.get(method) ?? []), f]),
    detach: async () => ws.close(),
  }
}

// The handler's response as plain parts: status, header pairs and the body's bytes.
async function respond(req: any): Promise<{ status: number; headers: [string, string][]; body: Uint8Array } | undefined> {
  for (const m of state.mocks.toReversed()) {
    if (!m.test.test(req.url)) continue
    const res = await m.handler(req)
    if (res === undefined) continue
    const raw = res.body
    const json = raw !== undefined && typeof raw !== "string" && !(raw instanceof Uint8Array)
    const body = raw === undefined ? new Uint8Array() : raw instanceof Uint8Array ? raw : new TextEncoder().encode(json ? JSON.stringify(raw) : raw)
    const out = { ...(json ? { "content-type": "application/json" } : {}), ...res.headers }
    return { status: res.status ?? 200, headers: Object.entries(out).map(([k, v]) => [k, String(v)] as [string, string]), body }
  }
  return undefined
}

// strobe's relay for an engine without a CDP session: one line per paused request, one line back.
// The request body comes as base64 where the browser collects it for strobe (BiDi's data collectors).
async function relay(port: number, token: string) {
  const sock = connect(port, "127.0.0.1")
  await new Promise((resolve, reject) => {
    sock.once("connect", resolve)
    sock.once("error", reject)
  })
  sock.setNoDelay(true)
  // strobe ends the relay with the block: a reset then is its close, not the bench's failure.
  sock.on("error", () => {})
  sock.write(token + "\n")
  const lines = createInterface({ input: sock })
  lines.on("error", () => {})
  lines.on("line", async (line) => {
    const r = JSON.parse(line)
    const body: string | undefined = r.body === undefined ? undefined : Buffer.from(r.body, "base64").toString()
    const req = { url: r.url, method: r.method, headers: r.headers ?? {}, body, json: () => JSON.parse(body ?? "null") }
    try {
      const res = await respond(req)
      sock.write(JSON.stringify(res ? { id: r.id, status: res.status, headers: res.headers, body: Buffer.from(res.body).toString("base64") } : { id: r.id, pass: true }) + "\n")
    } catch (error) {
      console.error(`mock ${r.method} ${r.url}: ${error instanceof Error ? error.stack : error}`)
      sock.write(JSON.stringify({ id: r.id, error: String(error) }) + "\n")
    }
  })
  return sock
}

async function answer(session: any, event: any) {
  const { request: r, requestId } = event
  const headers: Record<string, string> = r.headers ?? {}
  const body: string | undefined = r.postData ?? (r.postDataEntries ? r.postDataEntries.map((e: any) => Buffer.from(e.bytes ?? "", "base64").toString()).join("") : undefined)
  const req = { url: r.url, method: r.method, headers, body, json: () => JSON.parse(body ?? "null") }
  try {
    // The latest pattern answers first, and `undefined` passes to earlier ones, as Playwright's routes do.
    const res = await respond(req)
    if (!res) return await session.send("Fetch.continueRequest", { requestId })
    await session.send("Fetch.fulfillRequest", {
      requestId,
      responseCode: res.status,
      responseHeaders: res.headers.map(([name, value]) => ({ name, value })),
      body: Buffer.from(res.body).toString("base64"),
    })
  } catch (error) {
    console.error(`mock ${r.method} ${r.url}: ${error instanceof Error ? error.stack : error}`)
    await session.send("Fetch.failRequest", { requestId, errorReason: "Failed" }).catch(() => {})
  }
}

async function handle(op: string, args: any) {
  switch (op) {
    case "describe":
      return describe()
    // The block's fixture, before its `block`: hooks reach it as `ctx.fixture`.
    case "fixture":
      state.fixture = { url: args.url, port: args.port, pid: args.pid, home: args.home }
      return true
    // The experiment's seed, once, before the first block.
    case "seed":
      state.seed = args.seed >>> 0
      return true
    case "block":
      Object.assign(state, {
        caseSeed: mix(state.seed, `${args.case ?? ""}\n${canonical(args.params ?? {})}`),
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
        engine: args.engine ?? "chromium",
        relay: null,
        devtools: null,
      })
      return true
    case "before":
      stream("before")
      await spec.cases[args.case].launch.before?.(
        seeded({
          case: state.case,
          params: state.params,
          target: state.target,
          variant: state.variant,
          root: state.root,
          home: state.home,
          env: state.env,
          exe: state.exe,
          fixture: state.fixture,
        }),
      )
      return true
    case "prepare": {
      const target = spec.targets[state.target]
      launchHook = true
      try {
        if (target?.prepare) {
          state.devtools = args.endpoint ? { endpoint: args.endpoint, target: args.target } : null
          if (args.endpoint) state.playwright = await playwright(args.endpoint, args.target)
          stream("prepare")
          state.prepared = await target.prepare(prepareContext())
        }
        // A `url` function names the first page from what `prepare` returned, so the block loads one page.
        if (typeof target?.url === "function") stream("url")
        const url =
          typeof target?.url === "function"
            ? await target.url(seeded({ get case() { return caseOf() }, prepared: state.prepared, params: state.params }))
            : undefined
        return { url }
      } finally {
        launchHook = false
      }
    }
    case "setup":
      stream("setup")
      launchHook = true
      try {
        // Not spread: that would read `ctx.seed`.
        await spec.setup?.(Object.assign(context(), { addInitScript }))
      } finally {
        launchHook = false
      }
      return true
    case "run": {
      forCase(args.case)
      const run = hookFor(spec.cases[args.case].run, state.target)
      if (!run) throw new ParityGap(state.target, `run for ${args.case}`)
      stream("run")
      await run(context())
      return true
    }
    case "reset": {
      forCase(args.case)
      const c = spec.cases[args.case]
      const reset = hookFor(c.reset ?? (c.launch ? undefined : spec.reset), state.target)
      if (!reset) return false
      stream("reset")
      await reset(context())
      return true
    }
    case "close": {
      // Teardown hooks run while the browser is still open; one that throws does not keep the rest.
      const errors: unknown[] = []
      stream("teardown")
      await Promise.resolve(spec.teardown?.(context())).catch((e) => errors.push(e))
      stream("target-teardown")
      await Promise.resolve(spec.targets[state.target]?.teardown?.(prepareContext())).catch((e) => errors.push(e))
      if (state.session) await state.session.detach().catch(() => {})
      if (state.playwright) await state.playwright.browser.close().catch(() => {})
      if (state.relay) state.relay.end()
      Object.assign(state, { playwright: null, session: null, mocks: [], relay: null, devtools: null })
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
    // strobe's fix for a failed call (a missing permission, a selector to try) rides along as `hint`,
    // and the validator that refused it as `refusal`, which a hook that lets the error through hands back.
    if ("error" in message)
      waiting.reject(Object.assign(new Error(message.error), message.hint ? { hint: message.hint } : {}, message.refusal ? { refusal: message.refusal } : {}))
    else waiting.resolve(message.ok)
    return
  }
  // `used_random`: some hook so far read `ctx.seed` or called `ctx.random()` (the ledger's rule, SPEC §14.2).
  // `read_case`: a launch hook read `ctx.case` in this call (said once).
  const used = () => {
    const out = { ...(state.usedRandom ? { used_random: true } : {}), ...(readCase ? { read_case: true } : {}) }
    readCase = false
    return out
  }
  handle(message.op, message.args ?? {}).then(
    (ok) => write({ id: message.id, ok: ok ?? null, ...used() }),
    (error) =>
      write({
        id: message.id,
        ...used(),
        error: String(error?.message ?? error),
        stack: String(error?.stack ?? ""),
        ...(typeof error?.hint === "string" ? { hint: error.hint } : {}),
        ...(error?.refusal ? { refusal: error.refusal } : {}),
      }),
  )
})
write({ ready: true })
