// A fake tray app (dev only, not shipped): no window, says when it is up on stdout (a launch
// milestone), and wakes on a 250 ms timer like a tray app's tick. With `leak` it keeps 1 MB per tick.
setTimeout(() => console.log("fake ready"), 80)
let ticks = 0
const kept: Uint8Array[] = []
const leak = process.argv.includes("leak")
setInterval(() => {
  ticks++
  if (leak) kept.push(new Uint8Array(1 << 20).fill(ticks))
}, 250)
