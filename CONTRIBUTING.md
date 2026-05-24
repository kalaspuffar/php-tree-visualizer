# Contributing

This document is the developer-facing companion to [README.md](./README.md). If you're an operator running the collector, the README is where to start; if you're going to send patches, read on.

## Toolchain

- **Rust** — `stable`, pinned by [`rust-toolchain.toml`](./rust-toolchain.toml). The first `cargo` invocation installs the pinned toolchain via `rustup`. Components required by the gates below — `rustfmt` and `clippy` — are part of the default `stable` profile.
- **PHP 8.1 or newer** for the API code under [`api/`](./api/). Local PHP work uses the CLI; the deployed runtime is PHP-FPM behind a reverse proxy.
- **Linux x86_64.** All commands below are written for a POSIX shell on Linux. Contributors on macOS or other platforms may need to substitute distro-specific commands; we don't currently verify on anything other than Debian-derived Linux.

## Gates

Every commit MUST pass three gates locally before it lands. Run all three in order; do not skip.

```bash
cargo fmt --all
```

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

```bash
cargo test --all-targets --all-features
```

The `--all-targets` flag covers the library, the binary, every integration test under `crates/php-tree-viz-collector/tests/`, and the criterion benches in compile-only mode. If any of the three commands exits non-zero, the change is not ready for commit.

For the criterion benches themselves (separate from the gates above), see the `parse_batch_only` and `parse_and_record` cases in [`crates/php-tree-viz-collector/benches/decode_batch.rs`](./crates/php-tree-viz-collector/benches/decode_batch.rs). Run with `cargo bench --bench decode_batch`. These are not CI-gated; they exist to make regressions measurable across changes.

## OpenSpec workflow

Changes to this repository go through OpenSpec, a spec-first workflow that produces three artifacts (`proposal.md`, `design.md`, `tasks.md`) plus delta specs before any code lands. Three slash commands drive the loop:

- **`/opsx:propose <name>`** — scaffold a new change. Generates the four artifacts up front; you fill them in or the assistant does. Each capability you create or modify gets its own delta-spec file under `openspec/changes/<name>/specs/<capability>/spec.md`.
- **`/opsx:apply <name>`** — work through `tasks.md`, ticking items off as you complete them. Reads the proposal, design, and delta specs as context.
- **`/opsx:archive <name>`** — finalize a completed change after merge. Applies the delta specs against the main specs at `openspec/specs/<capability>/spec.md` and moves the change directory under `openspec/changes/archive/YYYY-MM-DD-<name>/`.

The OpenSpec config lives at [`openspec/config.yaml`](./openspec). Schema is `spec-driven`.

Role-specific prompts (Rust developer, web developer, UX designer, requirements analyst, solution architect, code reviewer) live in the `personas/` directory. The directory is intentionally gitignored — these are session-prompt material for focused work, not contributor-facing documentation. If you adopt one, load the relevant `personas/*.md` as the system prompt for that session.

## Handoff: push, review, merge, checkout main is manual

This repository follows a developer-stops-after-commit handoff. The split:

**The developer (you, or the assistant doing the work) per step:**

1. Branch from `main` (or from the previously completed step's branch if its PR is still open and the new work depends on it).
2. Open the OpenSpec change with `/opsx:propose`.
3. Implement the tasks.
4. Run the three gates (`fmt`, `clippy`, `test`).
5. Run `openspec validate <change-id>`.
6. Commit. Make small, focused commits with clear messages.
7. **Stop.** Report the branch name, the OpenSpec change ID, and a short summary.

**The operator per step:**

1. Push the feature branch.
2. Open the pull request.
3. Review.
4. Merge to `main`.
5. `git checkout main` and pull, so the next step branches off the merged state.

The developer does **not** push, merge, or switch branches. This is a hard rule and it prevents the "I merged my own PR and then realized something" failure mode. It also means a long-running feature with several `/opsx:propose → /opsx:apply` cycles produces several reviewable PRs, not one mega-PR.

## Wire-format authority

The on-the-wire MessagePack format the collector accepts (`php-analyze` schema v1) is co-owned with the upstream `php-analyze` project. When a question comes up about what a field means or whether a shape is allowed, the authority chain is fixed:

1. **The upstream `php-analyze` project's `SPECIFICATION.md` §4.2** wins. That's the canonical word.
2. **`crates/php-analyze/src/wire.rs`** (in the upstream repo) — the canonical Rust types the extension serializes from. If the spec is ambiguous, the types resolve it.
3. **`handover/WIRE_FORMAT.md`** (in this repo) — a digest produced for the visualizer team. Useful as a reference; subordinate to the two above.

If you find a contradiction between the three sources, file the discrepancy upstream and document the resolution. Do not silently choose one. See [SPECIFICATION.md §12.3](./SPECIFICATION.md) for the same chain stated from the visualizer side.

---

Back to [README.md](./README.md).
