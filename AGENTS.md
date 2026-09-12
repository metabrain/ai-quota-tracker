# AI Quota Tracker — Agent Working Agreement

This file is the single source of truth for agents working in this repository.
`CLAUDE.md` is a symlink to this file so Codex and Claude receive the same guidance.
An explicit current user instruction overrides this document.

## Project source of truth

`README.md` describes what this project is and how it must behave: the daemon model,
socket contract, environment variables, `/quota` response shape, and provider extension
model. Read it completely before changing code.

- If implementation and README disagree, treat that as a bug. Prefer fixing code to
  match the documented contract; when the documentation is wrong, update it in the same
  change.
- A behavior, socket API, environment-variable, or response-shape change is incomplete
  until the README reflects it.
- Document new providers, endpoints, and tunables in the same change that introduces
  them.

## Issue and scope discipline

Every bug fix, feature, or refactor must have a pre-existing GitHub Issue. If none
exists, create one and wait for it to exist before changing code. Keep one logical
change per issue.

- Name branches `<number>-short-slug`.
- Reference the issue in every commit and PR body with `Refs #<number>`, or use
  `Closes #<number>` for the resolving change.
- Link the issue from the PR and record exactly how the change was verified.
- Do only what the issue asks. Record unrelated findings as separate issues rather than
  expanding scope.
- Hard-to-reverse or outward-facing actions such as pushing, publishing, and deletion
  require explicit authorization unless the user has already authorized completing the
  relevant ticket workflow.

Required gates before opening or merging a PR are:

```text
cargo fmt --check
cargo clippy --all-targets
cargo test
cargo build --release
```

State exact results in the PR. If a gate was not run, say so.

## Foreman role

When the user asks an agent to act as foreman, manage tickets, resume work, or run the
development pipeline, the foreman acts as the project's control plane. It maintains the
authoritative view of work, delegates implementation, independently validates results,
and keeps ticket state truthful. The foreman does not perform delegated implementation.

GitHub Issues is the tracker. Before taking ticket action, establish the live state:

- List open issues, labels, severity, blockers, and update times.
- Inspect live Herdr agents, tabs, and workspaces before deciding that work is abandoned.
- Inspect git status, branches, and worktrees without disturbing user changes.
- Separate work already landed but awaiting evidence or closure from work needing code.
- Correct an issue body promptly when discovered facts invalidate its scope or premises;
  put verification evidence in comments.

Do not clear another orchestrator's sole claim merely because it appears old. Preserve
claim labels on closed tickets as ownership history. Follow any claim protocol documented
for this repository before planning, researching, dispatching, merging, or pushing a
ticket. This repository currently has no cross-orchestrator claim-label protocol; do not
invent one. An explicit user ticket assignment establishes foreman ownership unless a
future repository rule says otherwise.

## Planning and dispatch

Rank eligible work according to the repository's documented priority rules. Parallelize
only tickets whose mapped file sets and runtime resources are genuinely disjoint.

Delegate all ticket implementation through **Herdr in a new tab**. Never use the
orchestrator's native sub-agent spawning mechanism for repository implementation. Give
each ticket its own branch and isolated worktree, and assign distinct runtime ports or
other exclusive resources where relevant.

Keep cold-agent briefs short. The issue and repository guidance are the durable sources
of scope and acceptance criteria. Pass only dynamic routing facts:

```text
Work issue #<N> in your current worktree on branch <branch>.
Assigned resources: <ports or other exclusive resources>.
Foreman target: <target>. Report to <artifact path>, then send the short Herdr notification.
```

Add an overlap boundary or inaccessible-attachment observation only when it is specific
to that dispatch. Put missing decisions and acceptance criteria into the ticket rather
than a private one-off prompt.

Immediately after prompting, check the target's Herdr status once. `working` confirms
that processing began. If it remains `idle`, retry using Herdr's activity-confirming
wait; `agent_prompted` proves submission, not processing.

Before reusing an agent tab or session for another ticket, stop any active turn, clear
the prior conversation context using that agent's context-clear command, confirm it is
ready, and only then dispatch. A different tab or worktree does not itself make an old
session fresh.

## Agent handoffs

The implementer writes its detailed report and verification evidence to a shared
Markdown file or durable commit, then sends only a short attributed notification through
Herdr without `--wait`:

```text
[<agent>:#<ticket>] COMPLETE — report: <path>
```

Use `BLOCKED` or `FAILED` when appropriate. The implementer remains idle and available
for follow-up. Treat the notification as a signal, not as proof.

Do not routinely poll managed agents or read their transcripts. Leave them uninterrupted
until Herdr reports a blocked/input state, an agent sends a completion notification, or a
concrete coordination need arises. After 30 minutes with no signal, make one status or
transcript check to detect a silent stall.

## Independent review and verification

Before the foreman reads the completion report or implementation diff, dispatch a
different, fresh model/session through Herdr to review the handoff. The reviewer reads
the ticket, report, and actual diff and reports actionable findings. Use the current
repository model policy when one exists; do not rely on remembered model names.

The foreman then consumes the independent review and reproduces the load-bearing
mechanical verification. It does not perform the first-pass code review itself and never
substitutes an agent's green report for independent checks. If review finds defects,
return them to the implementer through Herdr and require another fresh review after the
correction.

Set verification to falsify the central claim:

- Make new guards or validation fire on specific malformed inputs.
- Compare exact before/after output for behavior-preserving refactors.
- Record before/after measurements for performance work.
- Verify UI work in a real browser and retain durable visual evidence.

Run the repository's complete required gates on the merged result, not only on the
ticket branch.

## Integration and cleanup

Before integration, revalidate ticket ownership under the repository's claim protocol
when one is configured.
Then merge, run the complete merged verification gates, push, post durable evidence,
correct related tickets whose facts changed, and close the ticket. Do not close work
that lacks reproducible evidence.

Remove only tabs, worktrees, and processes created for the delegated task. Stop services
by their assigned port or exact process identity; never use broad process-killing
patterns. Do not disturb a root-worktree service the user may be viewing.

Report outcomes first: what moved, what remains blocked, which evidence passed, and what
decision—if any—is needed from the user. Keep implementation mechanics in delegated
agent reports so the foreman retains context for coordination and judgment.
