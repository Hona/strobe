// The fixture of scripts/dev/fixture.bench.ts: answers HTTP on 127.0.0.1:<port>. With `copy` it first
// copies its runtime into <home> and serves from that copy (a fixture with an executable of its own),
// waiting until the copy listens and then for as long as it runs.
const [port, copy, home] = process.argv.slice(2)
if (copy === "copy") {
  const exe = `${home}/fixture-server${process.platform === "win32" ? ".exe" : ""}`
  require("fs").copyFileSync(process.execPath, exe)
  const child = Bun.spawn([exe, import.meta.path, port], { stdio: ["ignore", "pipe", "inherit"] })
  const reader = child.stdout.getReader()
  let seen = ""
  while (!seen.includes("listening")) {
    const { value, done } = await reader.read()
    if (done) process.exit(1)
    seen += new TextDecoder().decode(value)
  }
  console.log(`copy ${child.pid} listening on ${port}`)
  process.exit(await child.exited)
} else {
  Bun.serve({ hostname: "127.0.0.1", port: Number(port), fetch: () => new Response("ok") })
  console.log(`listening on ${port}`)
}
