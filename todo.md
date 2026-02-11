# TODO

## High Priority
- Fix CMake include wiring: `target_include_directories` currently receives `PkgConfig::...` targets as if they were paths. Keep include propagation via imported targets or extract real include dirs correctly.
- Add strict architecture mode (`--strict-arch`) to fail fast when a package is missing for any required arch (example: `icu` on `x86_64`).
- Improve clear-mode `remove`: avoid full resync where possible; perform incremental cleanup of only truly unused packages.

## Reliability
- Add download cache with checksum validation to avoid repeated archive downloads for the same package/version.
- Add end-of-command summary table for `add/remove`: package, version, found arches, headers, shared libs, skipped items.
- Add `doctor` command to validate `thirdparty` consistency (`.pc`, `.so`, include dirs, CMake/spec alignment) with actionable fixes.

## Testing
- Add integration tests for real template flow: `init-clear -> add -> rpmbuild` for `armv7`, `armv8`, `x86_64`.
- Add tests for strict-arch behavior and missing-arch failure reporting.

## CLI UX
- Add `--quiet`, `--verbose`, `--no-color`, and `--json` output modes.
- Standardize machine-readable error codes/messages for CI integration.
