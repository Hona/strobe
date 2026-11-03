// A fake tray app (dev only, not shipped): no window, says when it is up on stdout (a launch
// milestone), and wakes on a 250 ms timer like a tray app's tick.
setTimeout(() => console.log("fake ready"), 80)
let ticks = 0
setInterval(() => ticks++, 250)
