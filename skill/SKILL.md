---
name: strobe
description: Measure and explain performance of web pages, Electron apps (including the installed OpenCode desktop), Bun or Node processes and any process. Use when the user asks whether a change is faster, why something is slow, janky or heavy, where memory goes, or what the CPU or GPU is doing.
---

# Strobe

strobe records a target on one timeline and answers with verdicts and handles (`rec_…`, `run_…`, `fn_…`) to drill into. It refuses measurements it cannot defend: a refused run is an answer.

## Reference

    strobe status [target] [--calibrate]          environment, validity preflight, attachable targets
    strobe record <target> [--probes <set>] [--for 10s | --scenario <name>]
    strobe bench <scenario> --head <variant> [--base <variant>] [--primary <metric>] [--aa] [--bisect a..b]
        variant: a directory, a git ref, app:<id>, exe:<path>
    strobe query <handle | a..b | run_…> <view> [args]    views: summary frames threads hotspots stack
                                                          memory heap leaks gpu cpu insights timeline sql
    strobe report <view> <handle…> [--format md|html]     writes a file; show it in the Review pane

MCP: `status`, `record`, `bench`, `query`, `report`: same names, arguments and JSON as the CLI.

Targets: `https://…` · `cdp:<port>` · `app:opencode[-beta|-dev]` · `exe:<path>` · `node:<port>` · `pid:<n>` · `file:<path>` · `<target>@<device>` (mesh)

Probe sets: `timing` (default) · `attribution` (CPU stacks, allocation, invalidations, counters) · `memory` (census, heap snapshots) · `deep` (native stacks)

Exit codes: `0` · `1` error · `2` usage · `3` needs a human · `4` refused (invalid; the reason is in the result) · `5` inconclusive · `124` timeout · `255` unreachable

## Examples

    strobe bench timeline.scroll --base origin/dev --head . --primary frames.presented.p95
    strobe record app:opencode --for 10s
    strobe query rec_3f9a hotspots --by package --phase scroll --top 5
    strobe query rec_3f9a..rec_91c0 memory --by process
    bench { "scenario": "timeline.scroll", "base": "origin/dev", "head": "../feature" }
    query { "handle": "rec_3f9a", "view": "stack", "id": "fn_812" }

## Recipes

Did my change help? `bench` with one primary metric. Read the verdict, not the raw numbers.
Why is it slow? `record --probes attribution`, then `query frames`, then `query stack <id>`.
Where is the memory? `record --probes memory`, then `query memory --by process`.
Is it leaking? `bench <scenario> --head <variant> --probes memory --repeat 10`, then `query leaks`.
The installed OpenCode app: `status app:opencode` shows whether its bridge is on.
Which commit regressed? `bench <scenario> --bisect <good>..<bad>`.
Is the method sound here? `bench <scenario> --aa --head <variant>`.

## Notes

- Timing and attribution are separate rounds; never read timing from a `mixed` recording.
- `unclear` or `underpowered` means more pairs or a quieter machine, not "no change".
- Heap snapshots and traces can hold user content; views redact strings unless `--reveal`.
- Long benches keep running in the daemon: `query run_… progress`.
- If `strobe` isn't on PATH, use the CLI path from the MCP instructions.
