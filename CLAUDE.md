# GTC_Control — engineering rules

This file is the contract for how code is written in this repository. It overrides default behavior. Read `README.md` for product context.

## Language and platform

- **Rust only.** No shell scripts beyond what `cargo` and CI tooling provide. No Python helpers.
- **macOS only.** This is a deliberate scope choice. Do not introduce `cfg(target_os = ...)` branches for Linux or Windows, do not abstract over platforms "in case we ever port".
- **CLI + TUI.** The CLI (`main.rs`) and the full-screen TUI (`tui.rs`) are the two surfaces. Inner layers (`domain`, `modbus`, `config`, `app`) must stay UI-agnostic — both surfaces call the same operations and would equally serve any other future surface. No graphical-event-loop assumptions in inner layers.

## Idiomatic Rust — no fighting the compiler

- Write Rust the way the language wants to be written. If the borrow checker objects, the design is wrong — restructure the ownership, do not work around it.
- **Forbidden in application code:**
  - `unsafe` blocks. There is no FFI in this project.
  - `Rc<RefCell<T>>` / `Arc<Mutex<T>>` reached for as a first resort. Use them when shared mutable state is genuinely required (e.g. across async tasks); not as a way to avoid thinking about lifetimes.
  - `.unwrap()` and `.expect()` outside of tests and `main`. Use `?` and proper error types.
  - `clone()` to silence the borrow checker. Clone when ownership semantics genuinely require it, not as escape hatch.
- **Required:**
  - All errors flow through `thiserror`-defined error enums per module, surfaced as `Result<T, E>`. Application boundary (binary `main`) may collapse into `anyhow::Error` if useful — internal modules may not.
  - `&str` over `String` in function arguments where ownership is not taken.
  - Iterator chains over manual loops where the iterator form is at least as clear.
  - `Option` and `Result` combinators (`map`, `and_then`, `ok_or`) over manual `match` for two-armed cases.

## Architecture

Layering is enforced by module boundaries. Dependencies point only one way: outer layers depend on inner, never the reverse.

```
src/
  domain.rs    Pure logic: register definitions, value types, snapshots.
               No I/O. No async. Trivially unit-testable.
  config.rs    Bundled register catalogue (`config/default.yml`) merged
               with the user-editable `~/.gtc-control/config.yml`
               (Modbus endpoint, poll cadence, UI visibility toggles).
               Depends on: domain.
  modbus.rs    ModbusClient trait + tokio-modbus TCP implementation +
               FakeModbusClient for tests.
               Depends on: domain.
  app.rs       Orchestration: poll_once, read_one, set_value. Wires a
               ModbusClient + register map into the operations both the
               CLI and the TUI call.
               Depends on: domain, modbus.
  status.rs    Decoders that turn well-known register reads into the
               friendly view the TUI renders.
               Depends on: domain.
  tui.rs       Full-screen interactive view (ratatui + crossterm) and
               the Settings screen.
               Depends on: app, config, domain, status, modbus.
  lib.rs       Module declarations + CLI-side formatting helpers.
  main.rs      Composition root: CLI parsing (clap), tracing init,
               dispatch to app and tui.
```

- Each layer exposes a small public API; everything else is `pub(crate)` or private.
- Cross-cutting types (errors, IDs) live in `domain` or a dedicated `common` module — never duplicated.
- Traits are introduced when they enable testing or pluggability (the Modbus client), not preemptively.

## Testing

Code without tests is not done. This is non-negotiable.

- **Unit tests** live next to the code they test, in `#[cfg(test)] mod tests` blocks. Every public function in `domain/` and `config/` has unit tests covering happy path and at least one failure mode.
- **Integration tests** for `app/` use the `FakeModbusClient` fake to drive `poll_once` / `set_value` end-to-end without a real device.
- **Test fakes, not mocks.** A hand-written `FakeModbusClient` implementation in the `modbus` module. Avoid `mockall` and similar — they hide intent.
- **Property tests** via `proptest` for any code that parses or transforms data with non-trivial invariants (register value parsing, raw-word ↔ typed-value conversion).
- **No flaky tests.** A test that fails intermittently is a bug, not a flake. Investigate and fix or delete.
- **CI runs:** `cargo fmt --check`, `cargo lint`, `cargo test --all-features`, `cargo audit-strict`, `cargo deps-unused`.
- **Coverage is not a target metric** but `cargo llvm-cov` is set up so we can audit it. Aim for behaviors covered, not lines covered.

## Documentation

Standard Rust documentation conventions. This is not optional.

- Every public item (`pub fn`, `pub struct`, `pub enum`, `pub trait`, `pub mod`) carries a `///` doc comment.
- Module-level `//!` comments on every module explaining its responsibility and how it fits into the layering above.
- Doc comments follow the Rust API guidelines:
  - First line: a single-sentence summary, period-terminated.
  - Blank line, then expanded explanation if needed.
  - Sections: `# Examples`, `# Errors`, `# Panics` where applicable.
  - Examples in doc comments are runnable (`cargo test --doc` must pass).
- `# Errors` is required on every function returning `Result`.
- `# Panics` is required on every function that can panic (and panicking should be rare — see error-handling rule above).
- Cross-references use intra-doc links: `` [`Foo::bar`] ``, not free-form prose pointers.
- `README.md` covers: what the app does (one paragraph), how to build and run, how to fill in the register map. Nothing more.

## In-code comments

- Default to **no comment**. Names and types should carry the meaning.
- Write a comment only when removing it would lose information the reader cannot reconstruct from the code: a non-obvious invariant, a Modbus quirk, a performance constraint.
- Never narrate what the next lines do. Never paste ticket/PR numbers. Never reference a sibling file "for context" — the reader will grep.

## Formatting, lints, dependencies

- `rustfmt` with project defaults. No custom `rustfmt.toml` unless a real need appears.
- Edition: latest stable. MSRV: latest stable. We are not a library; pinning an old toolchain has no upside here.
- New dependencies are weighed. Prefer one well-maintained crate over three transient ones. Audit before adding anything that pulls in 200 transitive crates.

### Lint policy (deny-everything by default)

`Cargo.toml` configures rustc + clippy to deny rather than warn. A clean `cargo lint` run is the proof that a change is safe to commit. No `#[allow(...)]` sprinkled at use sites — if a rule needs a real exception, raise it for review and add an allowance in `Cargo.toml` (or, for unavoidable per-module exceptions, at the top of the module with a one-line rationale).

Denied at the rustc layer:
- `warnings` — promoted to errors. CI is the catch-net; local builds should be too.
- `missing_docs` — every public item documents itself.
- `unsafe_code` and `unsafe_op_in_unsafe_fn` — no FFI shortcuts in application code.

Denied at the clippy layer:
- `all`, `pedantic`, `cargo` (entire groups) — opinionated and load-bearing.
- `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented` — production code returns `Result`, not panics. Tests may opt in with `#[allow(...)]` at the test-module level.
- `dbg_macro`, `print_stdout`, `print_stderr` — use `tracing` for diagnostics; print macros leak into shipped builds. The CLI binary prints to stdout through a single dedicated `output` helper, allowed at that one site.
- `missing_errors_doc`, `missing_panics_doc` — public `Result`/`panic`-able functions document their failure modes.

Explicitly allowed (with rationale in `Cargo.toml`):
- `clippy::module_name_repetitions` — module-local error types per CLAUDE.md frequently repeat the module name; that is intentional.
- `clippy::multiple_crate_versions` — async / serde transitive dependency graphs occasionally pin different versions of small utility crates; not our problem to police.

### Cargo aliases (`.cargo/config.toml`)

The repo ships shared aliases so the commands referenced below are stable across machines:

- `cargo lint` → `cargo clippy --all-targets --all-features`. Full lint pass over lib, bins, examples, tests, and benches with every feature on. Use this, not bare `cargo clippy`.
- `cargo deps-unused` → `cargo machete`. Surfaces dependencies declared in `Cargo.toml` that no longer have a `use` referencing them. Run after removing modules.
- `cargo audit-strict` → `cargo audit --deny warnings` with a curated `--ignore` list for transitive advisories the project cannot patch directly. New advisories must be triaged: either fix (preferred) or add to the ignore list with a comment.

Per-user shell aliases are not a substitute — the `.cargo/config.toml` is the source of truth for what "all checks pass" means.

## Process

- Every change is reviewable as a diff. Commits are small and self-contained. No "WIP" or "fix" commits on `main`.
- Before any commit: `cargo fmt`, `cargo lint`, `cargo test`, `cargo audit-strict`, and `cargo deps-unused` all green locally.
- No commented-out code in commits. Delete it; `git log` is the archive.
- No `TODO` without an owner and a concrete next step. `// TODO: handle X` is forbidden. `// TODO(peter): retry with backoff on Modbus exception 0x0B` is fine.
