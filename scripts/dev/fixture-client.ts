// The app of scripts/dev/fixture.bench.ts. `use` holds a connection to the fixture; `own` starts a
// server of its own and talks only to that; `port` serves the fixture's port itself (on ::1) and talks
// to that.
const [arg, mode = "use"] = process.argv.slice(2)
let [host, port] = ["127.0.0.1", Number(arg)]
if (mode === "own") port = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch: () => new Response("mine") }).port
if (mode === "port") {
  Bun.serve({ hostname: "::1", port, fetch: () => new Response("mine") })
  host = "::1"
}
const socket = await Bun.connect({ hostname: host, port, socket: { data() {} } })
setInterval(() => socket.write("GET / HTTP/1.1\r\nHost: fixture\r\n\r\n"), 200)
console.log(`client ${mode} ready`)
