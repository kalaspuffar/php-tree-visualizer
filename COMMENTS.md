# COMMENTS.md

Supplementary notes, clarifications, and review comments that sit on top of
`SPECIFICATION.md` v0.1. When `SPECIFICATION.md` and this file conflict,
this file is treated as the more recent clarification — surface the
discrepancy before acting on it.

Append new entries at the bottom; do not edit history.

---

## 2026-05-23 — Placeholder

No supplementary notes at this time. `SPECIFICATION.md` v0.2 (with the
UX-augmented §3.3) is the working source of truth. Use the authority chain
in §1 for wire-format questions and the implementation phases in §10 for
ordering.

## 2026-05-23 — Workflow: push / review / merge / checkout main is manual

The end-of-step handoff is split between the Rust developer and the
operator (the human reviewer):

- **Rust developer's responsibilities, per step:** branch from `main`,
  open the OpenSpec change, implement, run `cargo fmt` / `cargo clippy
  --all-targets --all-features -- -D warnings` / `cargo test` /
  `openspec validate <change-id>`, commit, then stop and report the
  branch name, the OpenSpec change ID, and a short summary. The
  developer does **not** push, merge, or switch branches.
- **Operator's responsibilities, per step:** push the feature branch,
  open the pull request, review, merge to `main`, and `git checkout
  main`. The operator confirms completion before the developer starts
  the next step.

Implications for the developer:

- Treat `git push` and any operation on `main` as out of scope. If a
  push appears to be needed (e.g. CI on the branch), surface it as a
  question rather than acting.
- Each new step branches from `main` (which the operator has already
  fast-forwarded to include the previous step's merge). Do **not**
  branch from the previous step's local branch — that branch has been
  superseded by the merge commit on `main`.
- The OpenSpec change archive step (`openspec archive <change-id>`) is
  the developer's responsibility, but it happens after the merge —
  i.e. once back on `main` with the merge commit visible. Confirm with
  the operator before archiving if the timing is ambiguous.
