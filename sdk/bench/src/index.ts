// @hona/strobe: write benchmarks as small TypeScript files (SPEC §9.6).

/**
 * A visible element: a selector, or one whose text contains `text` (or is exactly it, trimmed). `index` (native apps)
 * takes that match, 0-based in tree order, among identical ones (six check boxes all named "checkbutton").
 */
export type Selector = string | { selector: string; text?: string; exact?: boolean; index?: number }

/** A page's CSS selector; on native apps a {@link Selector} object too. */
export type Element = Selector

/** When an input call resolves: `"idle"` (no DOM change or long frame for 300 ms), `"frame"` (after next paint), or a visible element. */
export type Until = "idle" | "frame" | Selector

export interface Options {
  until?: Until
  /** Milliseconds before the call fails the block. Default 30000. */
  timeout?: number
  /** Native targets: the window (a top-level window or overlay of the app) by its title, or part of it; default the main window first, then the app's other windows. */
  window?: string
}

/**
 * Drives the target. On a native target strobe drives the app through the OS's accessibility tree (UI Automation,
 * macOS AX, Linux AT-SPI) and input of the OS's own (Windows: posted messages, the app never takes focus; macOS:
 * Quartz events posted to the app, which comes to the front for keys; Linux: XTest): selectors are accessibility ids (an element key,
 * `"nav-history"`, or a key path, `"slider/knob"`), `role=button`, `text=General`, with a `:focus` suffix.
 * `goto` and `eval` are page-only; `act` and `query` are native-only.
 */
export interface Page {
  /** Opens `url`; a relative path resolves against the current page. */
  goto(url: string, opts?: Options): Promise<void>
  click(selector: Element, opts?: Options): Promise<void>
  /** Moves the pointer onto `selector` without a click: reveals hover-only controls, or parks the pointer. */
  hover(selector: Element, opts?: Options): Promise<void>
  /** Moves the pointer to a point in the viewport (CSS pixels), such as away from anything that reacts to hover. */
  move(x: number, y: number, opts?: Options): Promise<void>
  /** Focuses `selector` (clicking it if focus is elsewhere), then sends one key event per character. */
  type(selector: Element, text: string, opts?: Options & { delay?: number }): Promise<void>
  /** A key or chord: `"Enter"`, `"Control+a"`, `"ControlOrMeta+Shift+P"`. Native targets: `opts.selector` sends it to that element (clicked into focus first, as `type` does). */
  press(key: string, opts?: Options & { selector?: Element }): Promise<void>
  /**
   * Presses one key and holds it across other calls until `up`: a game's `"w"`, or `"Shift"` (held modifiers mark later keys).
   * A mouse button: `"Mouse"` (left), `"MouseRight"`, `"MouseMiddle"`, pressed where the pointer is, or first at `opts.at`
   * (a selector, or `[x, y]` in the viewport); `move` then drags.
   */
  down(key: string, opts?: Options & { at?: Element | [number, number] }): Promise<void>
  /** Lets go of a key or mouse button `down` pressed. */
  up(key: string, opts?: Options & { at?: Element | [number, number] }): Promise<void>
  /**
   * Presses at `selector`'s centre (where the pointer is with `""`), moves along `path` and releases. `path` is offsets
   * `[dx, dy]` from the start (CSS pixels; device-independent pixels on native apps), as keyframes spread evenly over
   * `duration` (ms, default 1000) or a function of `t` from 0 to 1 (a wiggle: `t => [40 * Math.sin(t * 12 * Math.PI), 0]`).
   * One move per display frame, paced on strobe's clock without waiting for the app to handle each; `input.rate` and
   * `input.gap.p95` say how evenly the moves went out. A button `down` already holds stays held.
   */
  drag(selector: Element, path: [number, number][] | ((t: number) => [number, number]), opts?: Options & { duration?: number; button?: "left" | "right" | "middle" }): Promise<void>
  /**
   * The page's pixels as `name`, for `--pixels`: an untimed block per variant captures each snapshot and strobe
   * compares base with head. Elsewhere it does nothing. Snapshot a state that does not depend on time (a still
   * page, an app's deterministic step), or the two builds differ for reasons of their own.
   */
  snapshot(name: string): Promise<void>
  scroll(selector: Element, delta: { by?: number; byX?: number; speed?: number }, opts?: Options): Promise<void>
  /** One mouse-wheel event of `dx`, `dy` (pixels; + is down and right) over `selector`, or the window's centre with `""`. */
  wheel(selector: Element, delta: { dx?: number; dy?: number }, opts?: Options): Promise<void>
  /** Native targets: shows, hides, minimizes, focuses or closes a window (`opts.window`, default the main one). `close` is the title bar's close (the app's own close path). */
  window(action: "show" | "hide" | "minimize" | "focus" | "close", opts?: Options): Promise<void>
  /**
   * Native targets: an accessibility action on an element (UI Automation's patterns): `invoke`, `toggle`, `setValue`
   * (`opts.value`), `expand`, `collapse`, `select`, `scrollIntoView`, `focus`. It skips hit testing and pointer
   * handling, so it is never an input latency (`interaction.*` refuses a run that only acted).
   */
  act(selector: Element, action: string, opts?: Options & { value?: string }): Promise<void>
  /** Native targets: the visible elements `selector` matches now: key, path, role, name, value, rect (screen pixels; points on macOS), focused, enabled, checked (a switch, check box, radio or toggle button's state; null for others). */
  query(selector: Element, opts?: Options): Promise<{ key: string; path: string; role: string; name: string; value?: string; rect: number[]; focused: boolean; enabled: boolean; checked: boolean | null }[]>
  /** Resolves when a visible element matches `selector` (holding `text`, if given). Re-checked on every DOM and focus change, so `:focus` works. */
  waitFor(selector: Element, opts?: { text?: string; exact?: boolean; timeout?: number }): Promise<void>
  /** Resolves when nothing in the DOM changed and no frame ran long for 300 ms. */
  idle(opts?: { timeout?: number }): Promise<void>
  wait(ms: number): Promise<void>
  /** Runs `fn` in the page. Only in `prepare`, `setup` and `reset`: it costs main-thread time. */
  eval<A extends unknown[], T>(fn: (...args: A) => T, ...args: A): Promise<Awaited<T>>
}

export interface Context<P> {
  page: Page
  /** The case this block runs: a block always belongs to one case. */
  case: string
  /** Marks a named span inside a run; every metric can be sliced by it, and `phase.<name>` is its wall time. */
  phase<T>(name: string, fn: () => T | Promise<T>): Promise<T>
  /** Adds `n` (default 1) to the `count.<name>` metric of the current run. Outside a run it counts nothing. */
  count(name: string, n?: number): void
  /** Calls an export of a `script()` target's module in its process and returns what it returned (JSON). */
  call<T = unknown>(name: string, ...args: unknown[]): Promise<T>
  params: P
  target: string
  variant: "base" | "head"
  /** The URL this block opened (empty for app, command and script targets). */
  url: string
  /** The variant's folder: a git ref's worktree, or the directory it names. */
  root: string
  /** App and exe targets: the private home the app runs in; null for web targets. Drive the app through its own files or CLI there (`Bun.spawn([ctx.exe, …], { env: ctx.env })`), never the user's. */
  home: string | null
  /** App and exe targets: the whole environment the app was launched with. */
  env: Record<string, string> | null
  /** App and exe targets: the executable this variant runs. */
  exe: string | null
  /** What the target's `prepare` returned for this block. */
  prepared: any
}

/** A request a `mock` handler answers. */
export interface MockRequest {
  url: string
  method: string
  headers: Record<string, string>
  /** The request body as text, if it had one. */
  body?: string
  json<T = any>(): T
}

/** A mocked response: `body` as text, bytes, or any other value as JSON. */
export interface MockResponse {
  status?: number
  headers?: Record<string, string>
  body?: string | Uint8Array | unknown
}

/** Answers a request, or returns `undefined` to let it through to the network. */
export type MockHandler = (request: MockRequest) => MockResponse | undefined | Promise<MockResponse | undefined>

export interface PrepareContext<P> {
  /** The case this block runs, so one file can give each case its own fixture. */
  case: string
  params: P
  target: string
  variant: "base" | "head"
  /** The URL strobe opens after `prepare`, such as the variant's server; mock its API under `new URL(url).origin`. */
  url: string
  /** The variant's folder: a git ref's worktree, or the directory it names. */
  root: string
  /** Playwright, connected to this block's browser. Only on targets with a debug port. */
  playwright: { browser: any; context: any; page: any }
  /**
   * Answers requests whose URL matches `pattern` (`*` is any run of characters) in this block's page.
   * Unlike Playwright's `page.route`, the browser's HTTP cache stays on: scripts and styles a reload
   * fetches come from the cache, as for a person. Later calls add patterns and are asked first; a
   * handler that returns `undefined` passes the request to earlier ones, as Playwright's routes do.
   */
  mock(pattern: string, handler: MockHandler): Promise<void>
}

/** What a launch's `before` hook gets: the launch has not started yet. */
export interface LaunchContext<P> {
  case: string
  params: P
  target: string
  variant: "base" | "head"
  /** The variant's folder: a git ref's worktree, or the directory it names. */
  root: string
  /** The private home the app launches in (its `HOME`, `APPDATA`, XDG folders); null for web targets. Ready before the hook: seed and profile files are in. */
  home: string | null
  /** App and exe targets: the whole environment the app launches with, to start what it expects to find (a background service) as it would. */
  env: Record<string, string> | null
  /** App and exe targets: the executable this variant launches. */
  exe: string | null
}

/** A point in a launch: the first line matching `match` in `in`, timed from process spawn. */
export interface Milestone {
  /** Text the line contains, or a regular expression it matches. A group named `at` holding epoch ms (`/entry: (?<at>\d{13})/`) is the time, for marks an app writes into its log; otherwise the time strobe read the line. */
  match: string | RegExp
  /** `"output"` (stdout and stderr, the default), `"page"` (the page's `performance.mark` names), or a glob of files under the launch's home. */
  in?: "output" | "page" | (string & {})
}

/** A native app's launch ends on its main window: `visible`, its pixels first `painted` (changed from its first frame), or `settled` (their last change, once they hold for a second). */
export interface WindowUntil {
  window: "visible" | "painted" | "settled"
}

/** A native launch ends on one of its own milestones (an output or log line): no window needed. */
export interface MilestoneUntil {
  milestone: string
}

export interface Launch<P> {
  /** The launch is ready when this is visible in any of its windows. The primary is `launch.ready`. A native target: the screen time of the frame that first showed the element in its accessibility tree (OS present events), or a window condition. */
  until: Selector | WindowUntil | MilestoneUntil
  /** Named points on the way, each a metric `launch.<name>`. One a case declares must appear in every launch. */
  milestones?: Record<string, Milestone>
  /** `fresh` (default): a new home every launch. `warm`: one home per series and variant, kept across its launches, so caches and state from earlier launches are there. */
  profile?: "fresh" | "warm"
  /** Before every launch, never timed: delete caches for a cold start, start or stop a service, seed the home. */
  before?(ctx: LaunchContext<P>): unknown
  /** Starts the main process with `--inspect` and reads Node's own bootstrap timing after the launch (`launch.node.*`). App and exe targets. */
  inspect?: boolean
  /** Times `launch.painted` and `launch.settled` from the window's pixels (Windows; default on for apps). */
  screen?: boolean
  /** Milliseconds before a launch that never shows `until` fails. Default 120000. */
  timeout?: number
}

interface CaseBase {
  /** The metric this case is about, declared before the run. */
  primary: string
  /** Extra secondary metrics to report: any catalog metric, `count.<name>`, `phase.<name>`, `net.requests{<url glob>}`, `layout.shift`, `launch.<milestone>`, `js.heap.retained` (scripts). */
  metrics?: string[]
  /** The targets this case runs on. Default: every target. */
  targets?: string[]
}

/** A case of runs inside an open target: measured `run`, unmeasured `reset`. */
export interface RunCase<P> extends CaseBase {
  /** Measured. */
  run(ctx: Context<P>): unknown
  /** Puts the app back between runs. Never measured. */
  reset?(ctx: Context<P>): unknown
  /** Milliseconds a run or reset may take before it fails the block (setup gets at least this too). Default 60000. */
  timeout?: number
  launch?: never
}

/** A case that measures the launch itself: one cold launch per block. */
export interface LaunchCase<P> extends CaseBase {
  launch: Launch<P>
  run?: never
  reset?: never
}

export type Case<P> = RunCase<P> | LaunchCase<P>

/** The first page a block opens: a path, or a function of what `prepare` returned. */
export type FirstPage<P> = string | ((ctx: { case: string; prepared: any; params: P }) => string | Promise<string>)

interface Hooks<P> {
  prepare?: (ctx: PrepareContext<P>) => unknown
  /** When the block closes, for what `prepare` started (a server, a temporary folder). */
  teardown?: (ctx: PrepareContext<P>) => unknown
}

export type Target<P> =
  | ({ kind: "web"; url: FirstPage<P>; viewport?: Viewport; scale?: number } & Hooks<P>)
  | ({ kind: "url"; href: string; viewport?: Viewport; scale?: number } & Hooks<P>)
  | { kind: "app"; id: string; home?: string; viewport?: Viewport; gpuCache?: GpuCache }
  | { kind: "exe"; path: string; app?: string; home?: string; viewport?: Viewport; gpuCache?: GpuCache }
  | ({ kind: "command" } & CommandOptions)
  | ({ kind: "script" } & ScriptOptions)
  | ({ kind: "native"; app?: string; path?: string } & NativeOptions)

/** Whether launches find the GPU shader caches earlier launches of the same variant compiled. */
export type GpuCache = "warm" | "cold"

export interface Viewport {
  width: number
  height: number
}

export interface CommandOptions {
  /** The command line, run by the shell in `cwd`, such as `bun run dev:desktop`. */
  run: string
  /** Relative to the variant's folder. Default: the folder itself. */
  cwd?: string
  /** Extra environment; `{home}` (the launch's private home) and `{port}` (a free port) are filled in. */
  env?: Record<string, string>
  /** Environment variables to remove; `PREFIX_*` removes a family. */
  unset?: string[]
  /** The text before the DevTools endpoint in the command's output. Default `"DevTools listening on"`. */
  attach?: string
  /** Milliseconds the command may take to print its endpoint. Default 600000. */
  timeout?: number
  /** Run once per variant before its first block, in its folder. */
  build?: string
  viewport?: Viewport
}

export interface ScriptOptions {
  /**
   * The harness module whose exports cases call (`ctx.call`), relative to the bench file. strobe
   * runs the same file for both variants, placed at the same path in a git ref's worktree, so its
   * relative imports reach that variant's code. Keep the harness in one file.
   */
  entry: string
  /** Default `"bun"`. */
  runtime?: "bun" | "node"
  env?: Record<string, string>
  /** Run once per variant before its first block, in its folder. */
  build?: string
}

export interface NativeOptions {
  /** A seed folder copied into each launch's fresh private home (`{param}` placeholders allowed), relative to the bench file. */
  home?: string
  /** The main window's client area in device-independent pixels, where the app lets strobe size it. */
  viewport?: Viewport
  /** Extra arguments after the profile's; `{home}` and `{port}` are filled in. */
  args?: string[]
  /** Extra environment after the profile's; `{home}` and `{port}` are filled in. */
  env?: Record<string, string>
  /** Variables the launch removes, after the profile's (`PREFIX_*` for a family): `["WAYLAND_DISPLAY"]` for an X11 run. */
  unset?: string[]
  /**
   * `"warm"` (default): each variant keeps the GPU driver's compiled shaders across its own launches (a discarded first
   * block fills them), as a person's machine has them; never the other variant's or yours. `"cold"`: every launch
   * starts without, so the driver compiles again (first launch after an install or a driver update).
   */
  gpuCache?: GpuCache

  /**
   * `false`: the app shows no window (a tray app). strobe neither waits for one nor places or checks it, launches end
   * on a milestone (`until: { milestone }`), and cases whose metrics are all CPU, memory, IO, counters and milestones
   * run while the session is locked.
   */
  window?: false
}

/** A parameter's values: all run, or `run` by default and `more` with `--params all`. */
export type ParamValues = readonly unknown[] | { readonly run: readonly unknown[]; readonly more?: readonly unknown[] }

type Values<T> = {
  [K in keyof T]: T[K] extends readonly (infer V)[]
    ? V
    : T[K] extends { run: readonly (infer V)[]; more: readonly (infer W)[] }
      ? V | W
      : T[K] extends { run: readonly (infer V)[] }
        ? V
        : never
}

export interface Bench<Params extends Record<string, ParamValues>> {
  targets: Record<string, Target<Values<Params>>>
  /** Every combination is its own series. */
  params?: Params
  /** Once per block, after load. Never measured. */
  setup?(ctx: Context<Values<Params>>): unknown
  /** Once per block, when it closes. Never measured. */
  teardown?(ctx: Context<Values<Params>>): unknown
  cases: Record<string, Case<Values<Params>>>
}

/** The project's production build, served per variant from `strobe.json`, in a strobe-launched Chromium. */
export function web<P = any>(opts: { url?: FirstPage<P>; viewport?: Viewport; scale?: number } & Hooks<P> = {}): Target<P> {
  return { kind: "web", url: opts.url ?? "/", prepare: opts.prepare, teardown: opts.teardown, viewport: opts.viewport, scale: opts.scale }
}

/** A fixed page; the same for both variants unless a variant is `url:<href>`. */
export function url<P = any>(href: string, opts: { viewport?: Viewport; scale?: number } & Hooks<P> = {}): Target<P> {
  return { kind: "url", href, ...opts }
}

/** An installed Electron app, by its profile in strobe.json `apps`, in a fresh isolated home seeded from `home`. */
export function app<P = any>(id: string, opts: { home?: string; viewport?: Viewport; gpuCache?: GpuCache } = {}): Target<P> {
  return { kind: "app", id, ...opts }
}

/** A packaged Electron app by path; `app` borrows a profile from strobe.json `apps` for it. */
export function exe<P = any>(path: string, opts: { app?: string; home?: string; viewport?: Viewport; gpuCache?: GpuCache } = {}): Target<P> {
  return { kind: "exe", path, ...opts }
}

/** An app started by a command in the variant's folder (a dev server that launches it), attached through the DevTools endpoint it prints. */
export function command<P = any>(run: string, opts: Omit<CommandOptions, "run"> = {}): Target<P> {
  return { kind: "command", run, ...opts }
}

/** A Bun or Node module of the variant, loaded in a fresh process per block; cases call its exports with `ctx.call`. */
export function script<P = any>(entry: string, opts: Omit<ScriptOptions, "entry"> = {}): Target<P> {
  return { kind: "script", entry, ...opts }
}

/**
 * A native app (no browser engine): by its profile in strobe.json `apps` (`kind: "native"`), or an executable path.
 * Each block launches it in a fresh isolated home and measures it from outside: CPU in cycles, thread wakes
 * (`cpu.wakes`), the UI thread (`ui.cpu`), IO, GPU, memory, what reached the screen (`frames.*`, `interaction.*`), and
 * for launch cases its window. `page` drives it through its accessibility tree. A directory or git-ref variant runs
 * strobe.json's `build` and launches its `output`.
 */
export function native<P = any>(appOrPath: string, opts: NativeOptions = {}): Target<P> {
  const path = /[\\/]|\.exe$/i.test(appOrPath)
  return { kind: "native", ...(path ? { path: appOrPath } : { app: appOrPath }), ...opts }
}

export function bench<const Params extends Record<string, ParamValues> = {}>(spec: Bench<Params>): Bench<Params> {
  return spec
}
