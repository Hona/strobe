// @strobe/bench: write benchmarks as small TypeScript files (SPEC §9.6).

/** When an input call resolves: `"idle"` (no DOM change or long frame for 300 ms), `"frame"` (after next paint), or a selector. */
export type Until = "idle" | "frame" | (string & {})

export interface Options {
  until?: Until
  /** Milliseconds before the call fails the block. Default 30000. */
  timeout?: number
}

export interface Page {
  /** Opens `url`; a relative path resolves against the current page. */
  goto(url: string, opts?: Options): Promise<void>
  click(selector: string, opts?: Options): Promise<void>
  /** Focuses `selector` (clicking it if focus is elsewhere), then sends one key event per character. */
  type(selector: string, text: string, opts?: Options & { delay?: number }): Promise<void>
  /** A key or chord: `"Enter"`, `"Control+a"`, `"ControlOrMeta+Shift+P"`. */
  press(key: string, opts?: Options): Promise<void>
  scroll(selector: string, delta: { by?: number; byX?: number; speed?: number }, opts?: Options): Promise<void>
  /** Resolves when a visible element matches `selector` (and contains `text`, if given). */
  waitFor(selector: string, opts?: { text?: string; timeout?: number }): Promise<void>
  /** Resolves when nothing in the DOM changed and no frame ran long for 300 ms. */
  idle(opts?: { timeout?: number }): Promise<void>
  wait(ms: number): Promise<void>
  /** Runs `fn` in the page. Only in `prepare`, `setup` and `reset`: it costs main-thread time. */
  eval<A extends unknown[], T>(fn: (...args: A) => T, ...args: A): Promise<Awaited<T>>
}

export interface Context<P> {
  page: Page
  /** Marks a named span inside a run; every metric can be sliced by it. */
  phase<T>(name: string, fn: () => T | Promise<T>): Promise<T>
  params: P
  target: string
  variant: "base" | "head"
  /** The URL this block opened (empty for app targets). */
  url: string
  /** What the target's `prepare` returned for this block. */
  prepared: any
}

export interface PrepareContext<P> {
  params: P
  target: string
  variant: "base" | "head"
  /** The URL strobe opens after `prepare`, such as the variant's server; mock its API under `new URL(url).origin`. */
  url: string
  /** Playwright, connected to this block's browser. Only on targets with a debug port. */
  playwright: { browser: any; context: any; page: any }
}

export interface Case<P> {
  /** The metric this case is about, declared before the run. */
  primary: string
  /** Extra secondary metrics to report. */
  metrics?: string[]
  /** Measured. */
  run(ctx: Context<P>): unknown
  /** Puts the app back between runs. Never measured. */
  reset?(ctx: Context<P>): unknown
}

export type Target<P> =
  | { kind: "web"; url: string; prepare?: (ctx: PrepareContext<P>) => unknown; viewport?: Viewport; scale?: number }
  | { kind: "url"; href: string; prepare?: (ctx: PrepareContext<P>) => unknown; viewport?: Viewport; scale?: number }
  | { kind: "app"; id: string; home?: string; viewport?: Viewport }
  | { kind: "exe"; path: string; home?: string; viewport?: Viewport }

export interface Viewport {
  width: number
  height: number
}

type Values<T> = { [K in keyof T]: T[K] extends readonly (infer V)[] ? V : never }

export interface Bench<Params extends Record<string, readonly unknown[]>> {
  targets: Record<string, Target<Values<Params>>>
  /** Every combination is its own series. */
  params?: Params
  /** Once per block, after load. Never measured. */
  setup?(ctx: Context<Values<Params>>): unknown
  cases: Record<string, Case<Values<Params>>>
}

/** The project's production build, served per variant from `strobe.json`, in a strobe-launched Chromium. */
export function web<P = any>(opts: { url?: string; prepare?: (ctx: PrepareContext<P>) => unknown; viewport?: Viewport; scale?: number } = {}): Target<P> {
  return { kind: "web", url: opts.url ?? "/", prepare: opts.prepare, viewport: opts.viewport, scale: opts.scale }
}

/** A fixed page; the same for both variants unless a variant is `url:<href>`. */
export function url<P = any>(href: string, opts: { prepare?: (ctx: PrepareContext<P>) => unknown; viewport?: Viewport; scale?: number } = {}): Target<P> {
  return { kind: "url", href, ...opts }
}

/** An installed Electron app, such as `opencode`, in a fresh isolated home seeded from `home`. */
export function app<P = any>(id: string, opts: { home?: string; viewport?: Viewport } = {}): Target<P> {
  return { kind: "app", id, ...opts }
}

/** A packaged Electron app by path. */
export function exe<P = any>(path: string, opts: { home?: string; viewport?: Viewport } = {}): Target<P> {
  return { kind: "exe", path, ...opts }
}

export function bench<const Params extends Record<string, readonly unknown[]> = {}>(spec: Bench<Params>): Bench<Params> {
  return spec
}
