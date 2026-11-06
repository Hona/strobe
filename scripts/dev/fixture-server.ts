// The fixture of scripts/dev/fixture.bench.ts: answers HTTP on 127.0.0.1:<port>. With `copy` it first
// copies its runtime into <home> and serves from that copy, as a fixture with an executable of its own.
const [port, copy, home] = process.argv.slice(2)
if (copy === "copy") {
  const exe = `${home}/fixture-server${process.platform === "win32" ? ".exe" : ""}`
  require("fs").copyFileSync(process.execPath, exe)
  Bun.spawn([exe, import.meta.path, port], { stdio: ["ignore", "inherit", "inherit"] })
} else {
  Bun.serve({ hostname: "127.0.0.1", port: Number(port), fetch: () => new Response("ok") })
}
