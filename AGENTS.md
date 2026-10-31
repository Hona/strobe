# strobe

- Read `SPEC.md` before any work. It is the source of truth. If a change conflicts with it, stop and ask. When the user agrees to a design change, update `SPEC.md` in the same change.
- Rule #1 is "valid or nothing" and rule #2 is "measure the app, not strobe" (SPEC §1). A feature that prints a number strobe cannot defend is a bug.
- Respect the module boundaries in SPEC §4.3: only `engine/` runs trace_processor, only `store/` writes `results.db` and `recordings/`, backends return `model` types.
- NEVER write automated tests: no unit, integration, e2e, or any other tests, no test files, no test dependencies, no CI test steps. Verify with the calibration page and A/A runs on real devices (SPEC §10.3, §10.4).
- Iteration speed first. Smallest change that works.
- Never experiment on the user's real app data or config. Attach to a running app only to read.
- NEVER launch an OpenCode desktop build (Stable, Beta, Dev) by hand or in code without the full isolation recipe in SPEC §13.2 (copied from `packages/desktop/script/bench-startup.ts`). `OPENCODE_DESKTOP_TEST_ROOT` alone replaced the user's live opencode service once: the desktop deletes `XDG_STATE_HOME` on Windows. Check the user's service pid before and after.
- Never run two benches on one device, and never bench while something heavy (a build, a test suite) runs on it.
- Rust stable. Well-known crates are fine; give each one a one-line reason in `Cargo.toml` and keep the budgets in SPEC §19.

## Dev environment (adais, Windows)

- Repo lives on the Dev Drive: `D:\Hona\strobe`. Sibling: `D:\Hona\mesh`.
- The shell PATH may be stale. Absolute paths: cargo `%USERPROFILE%\.cargo\bin`, gh `C:\Program Files\GitHub CLI`, git `C:\Program Files\Git\cmd`. opencode: the desktop install (mesh SPEC §6.4), not on PATH.
- OpenCode source for the bridge and scenarios: `D:\anomalyco` (worktrees under `D:\anomalyco\worktrees`).

## Devices for manual verification

Reach them with `mesh exec <device> …`.

| Mesh name | OS | Notes |
|---|---|---|
| adais | Windows | this machine: Ryzen 9 9950X3D (two CCDs, one with V-Cache), RTX 5070 Ti |
| blade | Windows | |
| air | macOS | often offline |
| vps | Linux | no display: only non-display metrics |
