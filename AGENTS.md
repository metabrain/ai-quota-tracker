# AGENTS.md

Agent-agnostic working rules for this repository. Any AI coding agent (Claude
Code, Cursor, Aider, Copilot, Codex, etc.) working here must follow these.

## 1. The README is the source of truth

`README.md` describes what this project **is** and how it is supposed to behave:
the daemon model, the socket contract, environment variables, the `/quota`
response shape, and how providers are added.

- Before changing code, read `README.md` and make your change conform to it.
- If the code and the README disagree, that is a bug. Prefer fixing the code to
  match the README. If the README is wrong or out of date, update it **in the
  same change** so the two never drift.
- Any change to behaviour, the socket API, env vars, or the response shape is not
  complete until `README.md` reflects it.
- New capabilities (new provider, new endpoint, new tunable) must be documented
  in `README.md` as part of the same change.

## 2. All work happens on a pre-existing Issue

Every bug fix, feature, or refactor must be tracked by a GitHub Issue that
exists **before** the work starts.

- No Issue? Open one first (`gh issue create`), describe the problem or goal, and
  wait for it to exist before writing code. Do not bundle "file the issue" into
  the same step as the fix.
- One Issue per logical change. Don't fix unrelated things in passing — open a
  separate Issue.
- Branch names and commits reference the Issue: branch `<number>-short-slug`,
  and every commit / PR body includes `Refs #<number>` (or `Closes #<number>`
  on the change that resolves it).
- The PR description links the Issue and explains how the change was verified.
- **Trivial fixes** (typos, comments, doc wording, obvious one-liners with no
  behaviour change) may be committed straight to `main` without a PR. They still
  reference an Issue with `Refs`/`Closes #<number>` and still keep `README.md`
  in sync. When in doubt, open a PR.

## 3. Verification

- `cargo build --release` and `cargo test` must pass before a PR is opened.
- `cargo clippy` and `cargo fmt --check` should be clean.
- State in the PR exactly what you ran and what the output was. If something was
  not run, say so.

## 4. Scope discipline

- Do only what the Issue asks. Surface anything else you notice as a new Issue
  rather than expanding the current change.
- Hard-to-reverse or outward-facing actions (pushing, publishing, deleting)
  need explicit sign-off unless already authorised.
