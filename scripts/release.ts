// Packs, or with --publish publishes, strobe's npm packages at Cargo.toml's version, in dependency order:
//   @hona/strobe-<platform>-<arch>  the binaries (strobe, strobew)
//   @hona/strobe                    the `strobe` command (a launcher for the platform package) and the bench SDK
//   @hona/strobe-bench              the SDK's old name, re-exporting @hona/strobe
//
//   bun scripts/release.ts --bin <folder with strobe.exe and strobew.exe> [--publish]
//
// A version already on npm is skipped, so a rerun after a failure publishes only what is missing.
import { $ } from "bun"
import { copyFileSync } from "node:fs"
import { join } from "node:path"
import { parseArgs } from "node:util"

const { values } = parseArgs({ options: { bin: { type: "string" }, publish: { type: "boolean", default: false } } })
const root = join(import.meta.dir, "..")
const cargo = await Bun.file(join(root, "Cargo.toml")).text()
const version = cargo.match(/^version = "([^"]+)"/m)?.[1]
if (!version) throw new Error("no version in Cargo.toml")

const platforms = [{ dir: "npm/strobe-win32-x64", files: ["strobe.exe", "strobew.exe"] }]
const packages = [...platforms.map((p) => p.dir), "sdk/bench", "npm/strobe-bench"]

// Every package, and every reference between them, names Cargo.toml's version.
for (const dir of packages) {
  const pkg = await Bun.file(join(root, dir, "package.json")).json()
  const refs = { ...pkg.dependencies, ...pkg.optionalDependencies }
  const wrong = [[pkg.name, pkg.version], ...Object.entries(refs).filter(([n]) => n.startsWith("@hona/strobe"))].filter(([, v]) => v !== version)
  if (wrong.length) throw new Error(`${dir}/package.json: ${wrong.map(([n, v]) => `${n} ${v}`).join(", ")}; Cargo.toml says ${version}`)
}

if (!values.bin) throw new Error("--bin <folder with the built binaries> is required")
for (const p of platforms) for (const f of p.files) copyFileSync(join(values.bin, f), join(root, p.dir, f))

// Without the platform packages: this release publishes them, so npm may not have them yet.
await $`bun install --frozen-lockfile --omit optional`.cwd(join(root, "sdk/bench"))
await $`bun run build`.cwd(join(root, "sdk/bench"))

for (const dir of packages) {
  const { name } = await Bun.file(join(root, dir, "package.json")).json()
  const published = (await $`npm view ${`${name}@${version}`} version`.quiet().nothrow()).stdout.toString().trim() === version
  if (published) {
    console.log(`${name}@${version}: already on npm`)
    continue
  }
  if (values.publish) await $`npm publish --access public`.cwd(join(root, dir))
  else await $`npm pack --dry-run`.cwd(join(root, dir))
}
