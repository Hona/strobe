// The script target of batch-more.bench.ts (not shipped): some work to time, and an export that ends the host.
let kept: number[] = []

export function work(n: number) {
  let x = 0
  for (let i = 0; i < n * 100_000; i++) x += Math.sqrt(i)
  return x
}

export function keep(n: number) {
  kept.push(...Array.from({ length: n }, (_, i) => i))
  return kept.length
}

export function clear() {
  kept = []
  return 0
}

export function exit() {
  process.exit(3)
}
