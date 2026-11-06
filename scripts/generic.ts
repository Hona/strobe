// Rule 15 (SPEC §1): strobe names no product it measures. Fails on a product's name in any tracked file
// outside the allowlist below. The Check workflow runs it.
//
//   bun scripts/generic.ts
import { $ } from "bun"
import { join } from "node:path"

const root = join(import.meta.dir, "..")

// Products strobe has measured or been asked about. Add a product here when one shows up in review.
const products: RegExp[] = [/\brune\b/i, /\bopencode\b/i, /\barcade\b/i, /\banomalyco\b/i, /\bkitlangton\b/i, /\bHex\b/, /\bhex#\d/, /\bapp:hex\b/, /\/hex\b/]

// The agent harness strobe registers its MCP and skill with (`up`, `leave`; SPEC §5.3) is the one product
// strobe may name, and only in that role: its CLI, its config and skill folders, its MCP locations.
const harness = /`opencode`|opencode( mcp| api|'s (global )?config| registration| MCP| locations)|opencode\/skills/gi

// Whole files allowed to name products, and why.
const allowed: Record<string, string> = {
  "AGENTS.md": "dev-environment notes: where consumer repos live on the dev machine",
  "src/app/up.rs": "registers strobe's MCP and skill with the agent harness",
  "scripts/generic.ts": "this list",
}

const files = (await $`git ls-files`.cwd(root).text()).split("\n").filter((f) => f && !(f in allowed))
const found: string[] = []
for (const file of files) {
  const bytes = new Uint8Array(await Bun.file(join(root, file)).arrayBuffer())
  if (bytes.includes(0)) continue
  new TextDecoder().decode(bytes).split("\n").forEach((line, i) => {
    const rest = line.replace(harness, "")
    for (const re of products) {
      const m = rest.match(re)
      if (m) found.push(`${file}:${i + 1}: "${m[0]}": ${line.trim().slice(0, 160)}`)
    }
  })
}
if (found.length) {
  console.error(`strobe is generic (SPEC §1 rule 15): no product names outside the allowlist in scripts/generic.ts.\n${found.join("\n")}`)
  process.exit(1)
}
console.log(`generic: ${files.length} files, no product names`)
