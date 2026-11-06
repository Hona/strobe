// @hona/strobe: write benchmarks as small TypeScript files (SPEC §9.6).

/**
 * A visible element, in one grammar for pages and native apps: a base (on a page a CSS selector, `>>>` crossing
 * shadow roots; on a native app an element key or key path, `"nav-history"`, `"slider/knob"`), then filters:
 * `role=<role>` (ARIA's role names), `text=<text>` (a substring of its text, label or value; `text==<text>` all of
 * it, trimmed; quotes hold spaces: `text="Copy session"`), `index=<n>` (that match, 0-based in tree order, among
 * identical ones: six check boxes all named "checkbutton"; negative counts from the end, `index=-1` the last) and a
 * `:focus` suffix. `'role=heading text=="General"'`, `".row text=Draft index=1"`. The object form says the same, its
 * `selector` in the string grammar. In an implementation comparison `@name` (optionally with filters after it,
 * `"@row index=-1"`) is the running target's {@link Side.ui} entry.
 */
export type Selector = string | { selector?: string; role?: string; text?: string; exact?: boolean; index?: number; focus?: boolean }

/** An element a call acts on: a {@link Selector}. */
export type Element = Selector

/** How a click presses: the button, the modifiers held around it (`"Shift"`, `"ControlOrMeta"`), and 2 for a double click, 3 for a triple. */
export interface ClickOptions extends Options {
  button?: "left" | "right" | "middle"
  modifiers?: ("Control" | "Shift" | "Alt" | "Meta" | "ControlOrMeta")[]
  count?: 1 | 2 | 3
}

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
 * Quartz events posted to the app, which comes to the front for keys; Linux: XTest). Selectors are one grammar on
 * both ({@link Selector}): their base is CSS on a page, an accessibility id on a native app. Keys are Playwright's
 * names from one table on every target (`"F9"`, `"PageUp"`, `"Delete"`, `"Control+Shift+P"`): a named key is that
 * key, never its name typed as text. `goto` and `eval` are page-only; `act` and `query` are native-only.
 */
export interface Page {
  /** Opens `url`; a relative path resolves against the current page. */
  goto(url: string, opts?: Options): Promise<void>
  /**
   * A single click is one `interaction.*` sample: from its press when the target changed between the press and the
   * release (it acts on the press: tabs, menus, many native controls; `interaction.n.press` counts them), else from
   * its release. Double and triple clicks are none. Chromium times its press and release events each from its own
   * trace, two samples.
   */
  click(selector: Element, opts?: ClickOptions): Promise<void>
  /** Moves the pointer onto `selector` without a click: reveals hover-only controls, or parks the pointer. */
  hover(selector: Element, opts?: Options): Promise<void>
  /** Moves the pointer to a point in the viewport (CSS pixels), such as away from anything that reacts to hover. */
  move(x: number, y: number, opts?: Options): Promise<void>
  /**
   * Focuses `selector` (clicking it if focus is elsewhere; `""` types where focus is), then sends one key event per character.
   * Paced: each key waits for the last one's frame and for the target to rest (at most 300 ms; `delay` counts from the key),
   * so each is an `interaction.*` sample; from OS presents where strobe follows them, else from the page's own frames
   * (headed Chromium, headless pages). Pacing stops for the rest of the call once the target never rests (an animation).
   * `burst: true` sends the keys `delay` apart without waiting: no key is a latency sample (`interaction.*` refuses
   * `AmbiguousInteraction`); measure burst typing by `cpu.work`, `main.work`, `ui.cpu`, `run.duration` or a phase.
   */
  type(selector: Element, text: string, opts?: Options & { delay?: number; burst?: boolean }): Promise<void>
  /** A key or chord: `"Enter"`, `"F9"`, `"Control+a"`, `"ControlOrMeta+Shift+P"`. `opts.selector` sends it to that element (clicked into focus first, as `type` does). */
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
   * Presses at the centre of the part of `selector` that shows (where the pointer is with `""`), moves along `path` and releases. `path` is offsets
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
  /** One mouse-wheel event of `dx`, `dy` (pixels; + is down and right) over `selector`, or the window's centre with `""`, with the keys `down` holds (`down("Control")` first: a zoom). */
  wheel(selector: Element, delta: { dx?: number; dy?: number }, opts?: Options): Promise<void>
  /** Native targets: shows, hides, minimizes, focuses or closes a window (`opts.window`, default the main one). `close` is the title bar's close (the app's own close path). */
  window(action: "show" | "hide" | "minimize" | "focus" | "close", opts?: Options): Promise<void>
  /**
   * Native targets: an accessibility action on an element (UI Automation's patterns): `invoke`, `toggle`, `setValue`
   * (`opts.value`), `expand`, `collapse`, `select`, `scrollIntoView`, `focus`. It skips hit testing and pointer
   * handling, so it is never an input latency (`interaction.*` refuses a run that only acted). `setValue` reads the value
   * back and fails when the app did not take it within a second.
   */
  act(selector: Element, action: "invoke" | "toggle" | "setValue" | "expand" | "collapse" | "select" | "scrollIntoView" | "focus", opts?: Options & { value?: string | number }): Promise<void>
  /** Native targets: the visible elements `selector` matches now: key, path, role, name, value, rect (screen pixels; points on macOS), focused, enabled, checked (a switch, check box, radio or toggle button's state; null for others). */
  query(selector: Element, opts?: Options): Promise<{ key: string; path: string; role: string; name: string; value?: string; rect: number[]; focused: boolean; enabled: boolean; checked: boolean | null }[]>
  /** Resolves when a visible element matches `selector` (holding `text`, if given). Re-checked on every DOM, tree and focus change, so `:focus` works. */
  waitFor(selector: Element, opts?: { text?: string; exact?: boolean; timeout?: number }): Promise<void>
  /** Resolves when nothing in the DOM changed and no frame ran long for 300 ms. */
  idle(opts?: { timeout?: number }): Promise<void>
  wait(ms: number): Promise<void>
  /** Runs `fn` in the page. Only in `prepare`, `setup` and `reset`: it costs main-thread time. */
  eval<A extends unknown[], T>(fn: (...args: A) => T, ...args: A): Promise<Awaited<T>>
}

export interface Context<P> extends Seeded {
  page: Page
  /**
   * The running target's adapter ({@link Side.ui}): a selector string, or a step called with this context
   * (`await ctx.ui.openSession()`). A name this target's `ui` lacks is a parity gap (`ParityGap`).
   */
  ui: Record<string, any>
  /** The case this block runs: a block always belongs to one case. */
  case: string
  /** Marks a named span inside a run; every metric can be sliced by it, and `phase.<name>` is its wall time. */
  phase<T>(name: string, fn: () => T | Promise<T>): Promise<T>
  /**
   * Leaves `fn`'s time out of the run: a wait between steps that is not the journey (`await ctx.untimed(() =>
   * page.idle())` between two clicks). Its wall time and the target's CPU in it (`cpu.*`, `ui.*`, `io.*`, `main.*`)
   * leave the run's totals, frames begun in it leave `frames.*`, `ctx.count` in it counts nothing, and inputs sent in
   * it are no `interaction.*` samples (an input just before it still is: its frame may come during it). For waits:
   * work started in it can leave a cost the run pays after it. A run total strobe cannot take a stretch out of
   * (`gpu.process.3d`, `energy.cpu`, `cpu.instructions`, `cpu.idle_wakes`, `net.*`, `canvas.*`, `js.heap.retained`)
   * is refused `Untimed` in a block whose runs used it. Outside a run (setup, reset) nothing is timed and it just
   * calls `fn`; one inside another, and `ctx.phase` inside one, are errors.
   */
  untimed<T>(fn: () => T | Promise<T>): Promise<T>
  /** Adds `n` (default 1) to the `count.<name>` metric of the current run. Outside a run it counts nothing. */
  count(name: string, n?: number): void
  /** Calls an export of a `script()` target's module in its process and returns what it returned (JSON). */
  call<T = unknown>(name: string, ...args: unknown[]): Promise<T>
  /**
   * `cli()` targets: runs a command to its exit in the variant's folder, timed and counted from outside (its CPU,
   * context switches, peak memory and IO, with everything it starts), and resolves with its exit code and output.
   * Default: the target's command (with a `cmd:` variant, the variant's). A nonzero exit fails the run. Lines it
   * prints as `strobe:metric <name>=<value>` add to `metric.<name>` for the run.
   */
  exec(line?: string): Promise<{ code: number; output: string }>
  /**
   * Native targets: resolves with the first line matching `match` that the app writes after the call, from its
   * output (`in: "output"`, the default; needs the target's `output: true`) or from files under its home matching a glob (`in: "logs/*.log"`), and
   * when strobe read it on its host clock (`at`, ns: the clock of phase marks and presented frames). Start the wait
   * before what makes the app write: `const saved = ctx.waitForLine(/saved/); await page.click("Save"); await saved`.
   * Fails after `timeout` ms (default 30000).
   */
  waitForLine(match: string | RegExp, opts?: { in?: string; timeout?: number }): Promise<{ line: string; at: number }>
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
  /** The bench file's {@link fixture}, running for this block: talk to it with `fetch(ctx.fixture.url + "/…")`. Null without one. */
  fixture: FixtureInfo | null
}

/**
 * Seeded workloads (a fuzzer, a random walk through an app). `strobe bench --seed <n>` replays one; without
 * it strobe draws a seed and prints it. strobe seeds only what the bench file asks for: never the app's own
 * `Math.random`, `crypto` or clock (pass `ctx.seed` in with `ctx.addInitScript` to seed the app's).
 */
export interface Seeded {
  /**
   * This case's seed (a 32-bit unsigned integer): from the experiment's seed, the case and its params, so base
   * and head (and both targets of a journey) get the same one. Reading it marks the workload as seeded.
   */
  seed: number
  /**
   * A number in [0, 1) from this hook's stream. Each hook call starts its stream over: every `run` from the same
   * point (so warmup converges and base and head run the same inputs), every `reset`, `setup`, `prepare`,
   * `before`, `url` function and teardown (the file's and the target's) from its own. Within one call each
   * draw is the next.
   */
  random(): number
}

/** A block's running fixture. */
export interface FixtureInfo {
  /** `http://127.0.0.1:<port>`. */
  url: string
  port: number
  pid: number
  /** Its own folder (`{home}` in its command): where a service it starts registers itself. */
  home: string
}

/** A request a `mock` handler answers. */
export interface MockRequest {
  url: string
  method: string
  headers: Record<string, string>
  /** The request body as text, if it had one (in Firefox, up to 16 MiB, where the browser collects it). */
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

export interface PrepareContext<P> extends Seeded {
  /** The case this block runs, so one file can give each case its own fixture. */
  case: string
  params: P
  target: string
  variant: "base" | "head"
  /** The URL strobe opens after `prepare`, such as the variant's server; mock its API under `new URL(url).origin`. */
  url: string
  /** The variant's folder: a git ref's worktree, or the directory it names. */
  root: string
  /**
   * Playwright, connected to this block's browser, when it is installed next to the bench file. Only on Chromium
   * targets; reading it on another engine throws (EngineUnsupported). Prefer `addInitScript` and `mock`, which work
   * in every engine.
   */
  playwright: { browser: any; context: any; page: any }
  /**
   * Answers requests whose URL matches `pattern` (`*` is any run of characters) in this block's page, in any engine.
   * Unlike Playwright's `page.route`, the browser's HTTP cache stays on: scripts and styles a reload
   * fetches come from the cache, as for a person. Later calls add patterns and are asked first; a
   * handler that returns `undefined` passes the request to earlier ones, as Playwright's routes do.
   */
  mock(pattern: string, handler: MockHandler): Promise<void>
  addInitScript: AddInitScript
}

/**
 * Runs `script` in every new document of the block's page before the page's own scripts (Playwright's
 * `page.addInitScript`, in any engine): a function is called with `arg` (JSON), a string runs as a function's body. Not in
 * the document already loaded. WebKit has no preload: there the script runs after the first document's own scripts,
 * and a block that loads another document is refused (EngineUnsupported).
 */
export type AddInitScript = <A = undefined>(script: string | ((arg: A) => unknown), arg?: A) => Promise<void>

/** `setup`'s context: a run's, plus {@link AddInitScript} for the documents the block loads next. */
export interface SetupContext<P> extends Context<P> {
  addInitScript: AddInitScript
}

/** What a launch's `before` hook gets: the launch has not started yet. */
export interface LaunchContext<P> extends Seeded {
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
  /** The bench file's {@link fixture}, already running (it starts before the app). */
  fixture: FixtureInfo | null
}

/** A point in a launch: the first line matching `match` in `in`, timed from process spawn. */
export interface Milestone {
  /** Text the line contains, or a regular expression it matches. A group named `at` holding epoch ms (`/entry: (?<at>\d{13})/`) is the time, for marks an app writes into its log; otherwise the time strobe read the line. */
  match: string | RegExp
  /** `"output"` (stdout and stderr, the default), `"page"` (the page's `performance.mark` names), or a glob of files under the launch's home. */
  in?: "output" | "page" | (string & {})
}

/** A native, app, exe or command launch ends on its main window: `visible`, its pixels first `painted` (changed from its first frame), or `settled` (their last change, once they hold for a second). Not a web target's (a page has no window of its own). */
export interface WindowUntil {
  window: "visible" | "painted" | "settled"
}

/** A native launch ends on one of its own milestones (an output or log line): no window needed. */
export interface MilestoneUntil {
  milestone: string
}

export interface Launch<P> {
  /** The launch is ready when this is visible in any of its windows. The primary is `launch.ready`. A page (Chromium or Firefox; WebKit refuses launch cases): from strobe's navigation, in a fresh profile, until its top document shows it. A native target: the screen time of the frame that first showed the element in its accessibility tree (OS present events). Any target with a window of its own may end on a window condition instead, read from its pixels the same way on every kind (two implementations timed alike). */
  until: Selector | WindowUntil | MilestoneUntil
  /** Named points on the way, each a metric `launch.<name>`. One a case declares must appear in every launch. */
  milestones?: Record<string, Milestone>
  /** `fresh` (default): a new home every launch. `warm`: one home per series and variant, kept across its launches, so caches and state from earlier launches are there. */
  profile?: "fresh" | "warm"
  /** Before every launch, never timed: delete caches for a cold start, start or stop a service, seed the home. */
  before?(ctx: LaunchContext<P>): unknown
  /** Starts the main process with `--inspect` and reads Node's own bootstrap timing after the launch (`launch.node.*`). App and exe targets. */
  inspect?: boolean
  /** Times `launch.painted` and `launch.settled` from the window's pixels (default on for apps; macOS needs Screen Recording). */
  screen?: boolean
  /** Milliseconds before a launch that never shows `until` fails. Default 120000. */
  timeout?: number
}

interface CaseBase {
  /** The metric this case is about, declared before the run. */
  primary: string
  /** Extra secondary metrics to report: any catalog metric, `count.<name>`, `phase.<name>`, `span.<name>` (a page's `performance.measure` spans), `energy.cpu`, `cpu.instructions`, `cpu.idle_wakes`, `net.requests{<url glob>}`, `layout.shift`, `launch.<milestone>`, `js.heap.retained` (scripts). */
  metrics?: string[]
  /** The targets this case runs on. Default: every target. In an implementation comparison a side whose target is not listed is a parity gap: the journey is shown, never run. */
  targets?: string[]
  /** Implementation comparisons: targets that cannot run this journey, with why (`{ rewrite: "no find in session yet" }`): a parity gap, listed in every report and never run on either side. */
  unsupported?: Record<string, string>
  /** Implementation comparisons: lead the report with this journey when it is decided. */
  hero?: boolean
  /** Non-inferiority: "is head at most this much worse?" instead of "did it change?". `"2%"` for the primary, or margins per metric (`{ "cpu.wakes": "5%" }`). Verdicts `no worse (≤2%)`, `worse (>2%)`, `unclear`. `--noworse` overrides. */
  noworse?: string | number | Record<string, string | number>
  /** What each block must show (over its target's `expect`); a block on another screen is refused `WrongScreen` instead of timed. */
  expect?: Expect
  /** App, exe and native targets: this case's GPU shader caches, over its target's and its app profile's `gpuCache`. */
  gpuCache?: GpuCache
  /**
   * Every run must start from the same state: strobe compares the values of editable controls and the
   * states of check boxes, radio buttons and switches before each run with the first run's, and refuses
   * `ResetIncomplete` when `reset` did not put them back. `false` turns the check off for a case whose
   * runs are meant to build on each other (say why in a comment).
   */
  resetCheck?: boolean
}

/**
 * The screen a block must show, read before its runs and after them (never inside one). Strings match
 * as substrings. Without it strobe still records what each block showed and warns `ScreenChanged`
 * when base and head (or some blocks) showed different screens.
 */
export interface Expect {
  /** A window title of the app (any visible window), or the page's title. */
  title?: string | RegExp
  /** An open dialog or a heading in the accessibility tree (a page's `h1`/`h2` or dialog). */
  heading?: string | RegExp
  /** A page's address. */
  url?: string | RegExp
  /** Elements that must be visible: accessibility selectors for a native app, CSS selectors for a page. */
  visible?: Element | Element[]
  /**
   * Implementation comparisons: elements (usually `@name`s) whose text or value (trimmed, whitespace collapsed) must
   * read the same on both sides after setup, after each run and after each reset; read outside the runs. A pair
   * whose sides read differently is refused `NotEquivalent`. Required with a per-target `run`.
   */
  same?: Element[]
}

/** A hook per target of an implementation comparison, by target name: for journeys whose steps differ entirely. */
export type PerTarget<P> = Record<string, (ctx: Context<P>) => unknown>

/** A case of runs inside an open target: measured `run`, unmeasured `reset`. */
export interface RunCase<P> extends CaseBase {
  /**
   * Measured. In an implementation comparison, `{ <target>: (ctx) => … }` gives each target its own steps
   * (`adapted: "steps"`); the case then needs `expect.same`, which shows the two step lists reach the same place.
   * A target with no entry is a parity gap.
   */
  run: ((ctx: Context<P>) => unknown) | PerTarget<P>
  /** Puts the app back between runs. Never measured. Per target like `run`. */
  reset?: ((ctx: Context<P>) => unknown) | PerTarget<P>
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

/** What a `url` function gets: the case, its params and what `prepare` returned; `random()` draws from its own stream. */
export interface FirstPageContext<P> extends Seeded {
  case: string
  prepared: any
  params: P
}

/** The first page a block opens: a path, or a function of what `prepare` returned. */
export type FirstPage<P> = string | ((ctx: FirstPageContext<P>) => string | Promise<string>)

interface Hooks<P> {
  prepare?: (ctx: PrepareContext<P>) => unknown
  /** When the block closes, for what `prepare` started (a server, a temporary folder). */
  teardown?: (ctx: PrepareContext<P>) => unknown
  /** What every block of this target must show (a case's `expect` adds to it). */
  expect?: Expect
}

export type Target<P> = Side &
  (
    | ({ kind: "web"; url: FirstPage<P> } & PageOptions & Hooks<P>)
    | ({ kind: "url"; href: string } & PageOptions & Hooks<P>)
    | ({ kind: "app"; id: string } & AppOptions)
    | ({ kind: "exe"; path: string; app?: string } & AppOptions)
    | ({ kind: "command" } & CommandOptions)
    | ({ kind: "script" } & ScriptOptions)
    | ({ kind: "cli" } & CliOptions)
    | ({ kind: "native"; app?: string; path?: string } & NativeOptions)
    | ({ kind: "webview"; app?: string; path?: string } & WebviewOptions)
  )

/**
 * What a target is as one side of an implementation comparison (`--base <target> --head <target>`):
 * its name in every report, and the project its builds and app profiles come from.
 */
export interface Side {
  /** What reports call this side ("Rewrite"); default the target's key. */
  name?: string
  /** A folder (relative to the bench file) whose strobe.json gives this target's `build` and app profiles: another repository's implementation. Its default variant is that project. (A `command` target's `cwd` is where its command runs.) */
  cwd?: string
  /**
   * The adapter: what a journey's `@name`s mean on this target. A selector string (CSS on a page, a key path or
   * `role=… text=…` on a native app), or a step `(ctx) => …` for where the steps differ (`ctx.ui.openSession()`).
   * A case that names an entry this target lacks (in its hooks, `expect` or `until`) is a parity gap: listed, never
   * run on either side.
   */
  ui?: Record<string, string | ((ctx: Context<any>) => unknown)>
}

/** Options of app and exe targets (Electron apps by profile or path). */
export interface AppOptions {
  /** A seed folder copied into each launch's fresh private home (`{param}` placeholders allowed), relative to the bench file. */
  home?: string
  viewport?: Viewport
  gpuCache?: GpuCache
  expect?: Expect
  /** Extra arguments after the profile's; `{home}`, `{port}` and `{fixture.url}`, `{fixture.port}`, `{fixture.pid}` are filled in. */
  args?: string[]
  /** Extra environment after the profile's; placeholders as in `args`. */
  env?: Record<string, string>
  /** Files written into the home before the launch, over the profile's (JSON or text; placeholders as in `args`; a string that is exactly `{fixture.port}` or `{fixture.pid}` becomes a number). */
  files?: Record<string, unknown>
}

/**
 * The browser a page runs in: `"chromium"` (default; over CDP, with its trace, profilers and heap snapshots),
 * `"firefox"` (over WebDriver BiDi: CPU, GPU, memory, counters, phases, network, and headed the OS's frames and
 * input latency; Chromium-only metrics are refused EngineUnsupported) or `"webkit"` (over WebDriver Classic:
 * Safari on macOS, headed; WebKitGTK headed or WPE headless on Linux; refused on Windows; no network events,
 * mocks or preload, so `net.*`, `ctx.mock` and the counters of a run that loads a page are refused).
 */
export type Engine = "chromium" | "firefox" | "webkit"

export interface PageOptions {
  viewport?: Viewport
  scale?: number
  /** The browser engine (`--engine` overrides it, or picks the targets of one engine). */
  engine?: Engine
  /** Run headless: no display needed (runs through a locked session); frame, input-latency and GPU-use metrics are refused (Headless). */
  headless?: boolean
}

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

export interface CliOptions {
  /**
   * The command line each `ctx.exec()` runs in the variant's folder, to its exit, at most the case's `timeout`.
   * Unix: `sh -c "exec <run>"` for one simple command, else `sh -c "<run>"` (assignments, pipes, `;`, `&&`,
   * redirects, builtins); Windows: the first word is the program (a `.cmd` or `.bat`, or a word that is no
   * executable, runs under cmd). Omit it when the variants are commands (`--base "cmd:<line>"`, what
   * `strobe run --base … --head …` does). `{fixture.url}` and the like are filled in.
   */
  run?: string
  /** Extra environment for the command (`{fixture.*}` filled in). */
  env?: Record<string, string>
  /** Run once per variant before its first block, in its folder. */
  build?: string
}

export interface NativeOptions {
  /** A seed folder copied into each launch's fresh private home (`{param}` placeholders allowed), relative to the bench file. */
  home?: string
  /** The main window's client area in device-independent pixels, where the app lets strobe size it. */
  viewport?: Viewport
  /** Extra arguments after the profile's; `{home}`, `{port}` and the fixture's `{fixture.url}`, `{fixture.port}`, `{fixture.pid}` are filled in. */
  args?: string[]
  /** Extra environment after the profile's; placeholders as in `args`. */
  env?: Record<string, string>
  /** Files written into the home before the launch, over the profile's (JSON or text; placeholders as in `args`). */
  files?: Record<string, unknown>
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
  /** What every block of this target must show (a case's `expect` adds to it). */
  expect?: Expect
  /**
   * `true`: strobe reads the app's stdout and stderr through a pipe, for `ctx.waitForLine` on its output. Off by
   * default, as a pipe changes how a C runtime buffers stdout: an app whose output a case waits on must flush each line.
   */
  output?: boolean
}

export interface WebviewOptions {
  /** A seed folder copied into each launch's fresh private home (`{param}` placeholders allowed), relative to the bench file. */
  home?: string
  /** The page's size in CSS pixels: strobe sizes the app's window to it, and refuses an app that keeps its size. */
  viewport?: Viewport
  /** As for `native`: whether launches find the GPU shader caches (the driver's and the webview's) earlier launches compiled. */
  gpuCache?: GpuCache
  /** What every block of this target must show (a case's `expect` adds to it). */
  expect?: Expect
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
  /** A process every block talks to (a server with seeded data): see {@link fixture}. */
  fixture?: Fixture
  /** Once per block, after load. Never measured. */
  setup?(ctx: SetupContext<Values<Params>>): unknown
  /** Once per block, when it closes. Never measured. */
  teardown?(ctx: Context<Values<Params>>): unknown
  cases: Record<string, Case<Values<Params>>>
}

/**
 * The project's production build, served per variant from `strobe.json`, in a strobe-launched Chromium.
 * A page needs no `gpuCache` choice: each block gets a fresh browser profile (a cold first visit for the
 * page's own caches) whose warmup runs fill Chromium's GPU caches before the measured runs, and the GPU
 * driver's own caches are the user's, as for any browser on the machine (SPEC §13.2).
 */
export function web<P = any>(opts: { url?: FirstPage<P> } & PageOptions & Hooks<P> & Side = {}): Target<P> {
  return { kind: "web", url: opts.url ?? "/", prepare: opts.prepare, teardown: opts.teardown, viewport: opts.viewport, scale: opts.scale, expect: opts.expect, name: opts.name, cwd: opts.cwd, engine: opts.engine, headless: opts.headless, ui: opts.ui }
}

/** A fixed page; the same for both variants unless a variant is `url:<href>`. */
export function url<P = any>(href: string, opts: PageOptions & Hooks<P> & Side = {}): Target<P> {
  return { kind: "url", href, ...opts }
}

/** An installed Electron app, by its profile in strobe.json `apps`, in a fresh isolated home seeded from `home`. */
export function app<P = any>(id: string, opts: AppOptions & Side = {}): Target<P> {
  return { kind: "app", id, ...opts }
}

/** A packaged Electron app by path; `app` borrows a profile from strobe.json `apps` for it. */
export function exe<P = any>(path: string, opts: { app?: string } & AppOptions & Side = {}): Target<P> {
  return { kind: "exe", path, ...opts }
}

/** An app started by a command in the variant's folder (a dev server that launches it), attached through the DevTools endpoint it prints. */
export function command<P = any>(run: string, opts: Omit<CommandOptions, "run"> & Side = {}): Target<P> {
  return { kind: "command", run, ...opts }
}

/** A Bun or Node module of the variant, loaded in a fresh process per block; cases call its exports with `ctx.call`. */
export function script<P = any>(entry: string, opts: Omit<ScriptOptions, "entry"> & Side = {}): Target<P> {
  return { kind: "script", entry, ...opts }
}

/**
 * A command run to its exit per `ctx.exec()` (a CLI, a compiler, a test binary), measured from outside: `cpu.work`
 * (its CPU with everything it starts), `run.duration` (spawn to exit), `cli.launch` (the OS starting it, on
 * strobe's thread), `cpu.wakes`, `memory.peak`, `io.read`,
 * `io.write`, and `metric.<name>` from its `strobe:metric <name>=<value> [lower|higher]` lines (default lower is
 * better; every command of a run must print each name it prints, in one direction). Cases: `run: (ctx) => ctx.exec()`.
 */
export function cli<P = any>(run?: string, opts: Omit<CliOptions, "run"> & Side = {}): Target<P> {
  return { kind: "cli", run, ...opts }
}

/**
 * A native app (no browser engine): by its profile in strobe.json `apps` (`kind: "native"`), or an executable path.
 * Each block launches it in a fresh isolated home and measures it from outside: CPU in cycles, thread wakes
 * (`cpu.wakes`), the UI thread (`ui.cpu`), IO, GPU, memory, what reached the screen (`frames.*`, `interaction.*`), and
 * for launch cases its window. `page` drives it through its accessibility tree. A directory or git-ref variant runs
 * strobe.json's `build` and launches its `output`.
 */
export function native<P = any>(appOrPath: string, opts: NativeOptions & Side = {}): Target<P> {
  const path = /[\\/]|\.exe$/i.test(appOrPath)
  return { kind: "native", ...(path ? { path: appOrPath } : { app: appOrPath }), ...opts }
}

/**
 * An app that embeds the OS's webview, by its profile in strobe.json `apps` (any `kind`), or an executable path.
 * Each block launches it in a fresh isolated home with the webview's own debugging port open, and `page` drives
 * its page as a web target's: page metrics, attribution and the app's whole process set (host and webview
 * processes). Windows (WebView2) only for now; elsewhere it is refused `EngineUnsupported`, and `native()` measures the app through its
 * accessibility tree. A directory or git-ref variant runs strobe.json's `build` and launches its `output`.
 */
export function webview<P = any>(appOrPath: string, opts: WebviewOptions & Side = {}): Target<P> {
  const path = /[\\/]|\.exe$/i.test(appOrPath)
  return { kind: "webview", ...(path ? { path: appOrPath } : { app: appOrPath }), ...opts }
}

/** A fixture: the command and how strobe starts it. */
export interface Fixture {
  /** Run by the shell in the bench file's folder; `{port}` (a free port, required), `{home}` (its own folder) and `{<param>}` are filled in. */
  command: string
  /** A folder (relative to the bench file, `{param}` placeholders allowed) copied into the fixture's home for every block; its content hash is in result.json. */
  seed?: string
  /** Extra environment; placeholders as in `command`. */
  env?: Record<string, string>
  /** A URL that answers once it is ready (`http://127.0.0.1:{port}/health`); default: its port accepts a connection. */
  ready?: string
  /** Milliseconds it may take to be ready. Default 60000. */
  timeout?: number
}

/**
 * A process both sides of a comparison talk to, such as a server with seeded data. strobe starts it
 * for every block before the target, in a group and home of its own (so its CPU is in neither side's
 * numbers: it is `cpu.fixture`), fills `{fixture.url}`, `{fixture.port}` and `{fixture.pid}` into the
 * targets' `args`, `env` and `files`, gives hooks `ctx.fixture`, and ends it with the block. A block
 * whose app never connects to it, or runs its executable itself, is refused `FixtureMismatch`.
 */
export function fixture(command: string, opts: Omit<Fixture, "command"> = {}): Fixture {
  return { command, ...opts }
}

export function bench<const Params extends Record<string, ParamValues> = {}>(spec: Bench<Params>): Bench<Params> {
  return spec
}
