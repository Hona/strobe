// Packs, or with --publish publishes, strobe's npm packages at Cargo.toml's version, in dependency order:
//   @hona/strobe-<platform>-<arch>  the binaries (strobe, strobew)
//   @hona/strobe                    the `strobe` command (a launcher for the platform package) and the bench SDK
//   @hona/strobe-bench              the SDK's old name, re-exporting @hona/strobe
//
//   bun scripts/release.ts --bin <folder> [--publish]
//
// <folder> holds one subfolder per platform with its built binaries, as release.yml's build jobs leave
// them: win32-x64/{strobe.exe,strobew.exe}, darwin-arm64/strobe, darwin-x64/strobe, linux-x64/strobe,
// linux-arm64/strobe. Every platform must be there: a release never ships without one.
// A version already on npm is skipped, so a rerun after a failure publishes only what is missing.
// With --publish each package waits until npm serves the ones it depends on (`waitVisible`).
import { $ } from "bun"
import { chmodSync, copyFileSync } from "node:fs"
import { join } from "node:path"
import { parseArgs } from "node:util"

// --otp: npm's one-time password for an account with two-factor publishing (CI uses trusted publishing instead).
const { values } = parseArgs({ options: { bin: { type: "string" }, publish: { type: "boolean", default: false }, otp: { type: "string" } } })
const root = join(import.meta.dir, "..")
const cargo = await Bun.file(join(root, "Cargo.toml")).text()
const version = cargo.match(/^version = "([^"]+)"/m)?.[1]
if (!version) throw new Error("no version in Cargo.toml")

const platforms = [
  { target: "win32-x64", files: ["strobe.exe", "strobew.exe"] },
  { target: "darwin-arm64", files: ["strobe"] },
  { target: "darwin-x64", files: ["strobe"] },
  { target: "linux-x64", files: ["strobe"] },
  { target: "linux-arm64", files: ["strobe"] },
].map((p) => ({ ...p, dir: `npm/strobe-${p.target}` }))
const packages = [...platforms.map((p) => p.dir), "sdk/bench", "npm/strobe-bench"]

// Every package, and every reference between them, names Cargo.toml's version.
for (const dir of packages) {
  const pkg = await Bun.file(join(root, dir, "package.json")).json()
  const refs = { ...pkg.dependencies, ...pkg.optionalDependencies }
  const wrong = [[pkg.name, pkg.version], ...Object.entries(refs).filter(([n]) => n.startsWith("@hona/strobe"))].filter(([, v]) => v !== version)
  if (wrong.length) throw new Error(`${dir}/package.json: ${wrong.map(([n, v]) => `${n} ${v}`).join(", ")}; Cargo.toml says ${version}`)
}

if (!values.bin) throw new Error("--bin <folder with the built binaries> is required")
for (const p of platforms)
  for (const f of p.files) {
    const to = join(root, p.dir, f)
    copyFileSync(join(values.bin, p.target, f), to)
    // Artifacts lose the executable bit on the way; npm packs the mode it finds.
    if (!f.endsWith(".exe")) chmodSync(to, 0o755)
  }

// Without the platform packages: this release publishes them, so npm may not have them yet.
await $`bun install --frozen-lockfile --omit optional`.cwd(join(root, "sdk/bench"))
await $`bun run build`.cwd(join(root, "sdk/bench"))

// Whether installers see `name@version`: npm's answer (`npm view`, which reads the full document) and the
// abbreviated document package managers install from, which the registry's CDN serves on its own schedule.
// Right after 0.11.0 was published, a `bun install` wrote a lockfile without one platform package.
async function visible(name: string) {
  const viewed = (await $`npm view ${`${name}@${version}`} version --prefer-online`.quiet().nothrow()).stdout.toString().trim() === version
  if (!viewed) return false
  const url = `https://registry.npmjs.org/${name.replace("/", "%2f")}`
  const doc = await fetch(url, { headers: { accept: "application/vnd.npm.install-v1+json" }, cache: "no-store" }).then((r) => (r.ok ? r.json() : null)).catch(() => null)
  return !!doc?.versions?.[version]
}

// Until every package of `dirs` is visible, or 15 minutes: a package that depends on them is published only then,
// so no install of it resolves while one of them is missing.
async function waitVisible(dirs: string[]) {
  const names: string[] = await Promise.all(dirs.map(async (dir) => (await Bun.file(join(root, dir, "package.json")).json()).name))
  const deadline = Date.now() + 15 * 60_000
  for (;;) {
    const missing = (await Promise.all(names.map(async (n) => ((await visible(n)) ? null : n)))).filter(Boolean)
    if (!missing.length) return console.log(`visible on npm at ${version}: ${names.join(", ")}`)
    if (Date.now() > deadline) throw new Error(`not visible on npm after 15 minutes: ${missing.join(", ")}@${version}; rerun the release to publish the rest once they are`)
    console.log(`waiting for npm to serve ${missing.join(", ")}@${version}`)
    await Bun.sleep(15_000)
  }
}

// Each package after the ones it depends on are visible: the platform packages, then @hona/strobe (which
// names them as optional dependencies), then @hona/strobe-bench (which depends on @hona/strobe).
const stages = [platforms.map((p) => p.dir), ["sdk/bench"], ["npm/strobe-bench"]]
for (const [i, stage] of stages.entries()) {
  if (values.publish && i > 0) await waitVisible(stages[i - 1])
  for (const dir of stage) {
    const { name } = await Bun.file(join(root, dir, "package.json")).json()
    const published = (await $`npm view ${`${name}@${version}`} version --prefer-online`.quiet().nothrow()).stdout.toString().trim() === version
    if (published) {
      console.log(`${name}@${version}: already on npm`)
      continue
    }
    const otp = values.otp ? ["--otp", values.otp] : []
    if (values.publish) await $`npm publish --access public ${otp}`.cwd(join(root, dir))
    else await $`npm pack --dry-run`.cwd(join(root, dir))
  }
}
if (values.publish) await waitVisible(stages.flat())
