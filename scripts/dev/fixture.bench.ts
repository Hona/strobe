// Dev bench for the fixture's ownership checks (not shipped), windowless, so it runs on a locked session.
// From the repo root:
//   strobe bench scripts/dev/fixture.bench.ts --head . --check --target use    passes, connected
//   strobe bench scripts/dev/fixture.bench.ts --head . --check --target own    FixtureMismatch: no connection the fixture answered
//   strobe bench scripts/dev/fixture.bench.ts --head . --check --target port   FixtureMismatch: served the fixture's port itself
// STROBE_DEV_FIXTURE=copy serves the fixture from a copy of its runtime, which the app's runtime matches byte
// for byte: every target is then refused FixtureMismatch (a copy of the fixture's executable).
import { bench, fixture, native } from "@hona/strobe"

const client = (mode: string) => native("fake", { args: [import.meta.dir + "/fixture-client.ts", "{fixture.port}", mode], window: false, output: true })

export default bench({
  fixture: fixture(process.env.STROBE_DEV_FIXTURE === "copy" ? "bun fixture-server.ts {port} copy {home}" : "bun fixture-server.ts {port}"),
  targets: { use: client("use"), own: client("own"), port: client("port") },
  cases: {
    idle: { primary: "cpu.work", setup: ({ page }) => page.wait(1000), run: ({ page }) => page.wait(500) },
  },
})
