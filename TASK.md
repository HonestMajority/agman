# Goal
Add a 3-second buffer between "session first observed ready" and "deliver" in the inbox poller.

## Problem

Task agents miss inbox messages on cold start: the supervisor kills claude, queues the work directive, and relaunches. The poller's readiness gate (pane_current_command != shell) flips to ready ~50ms after the new node process starts — before Ink has mounted its input handler. The paste lands in claude's stdin buffer; the trailing Enter keystrokes are absorbed by the still-mounting UI; the message sits unsubmitted in the input field. CEO/PM/researcher avoid this only because their messages typically arrive long after warm-up.

## Fix

Add a 3-second buffer between "session first observed ready" and "deliver." Applies uniformly to all targets — costs nothing for warm chats (buffer elapsed long ago), reliably covers cold-start for task agents.

## Plan

### Completed
- [x] Add `first_ready_at: HashMap<String, Instant>` field to `App` struct alongside `stuck_skip_counts` (with doc comment).
- [x] Initialize empty in `App::new()`.
- [x] Add `first_ready_at` field to `InboxPollOutput`.
- [x] Clone `self.first_ready_at` into the spawned poll task and pass it back via `InboxPollOutput`.
- [x] Write `output.first_ready_at` back to `self.first_ready_at` in `apply_inbox_poll_results`.
- [x] Restructure per-target loop in `start_inbox_poll`: readiness+buffer → already-pasted rescue → delivery (was: rescue → readiness → delivery).
- [x] Gate `Ok((true, _))` arm on a 3s buffer using `first_ready_at.entry(target.clone()).or_insert_with(Instant::now)`.
- [x] In `Ok((false, _))` arm, `first_ready_at.remove(&target)` so kill+relaunch re-arms the buffer.
- [x] Update fallback `InboxPollOutput` (on `JoinError`) to include empty `first_ready_at`.
- [x] `cargo build --release` clean (only the pre-existing dead-code warning in `vim.rs`).
- [x] `cargo nextest run` — 257/257 pass.

### Remaining

- [ ] Manual verification (out of automated loop): kill+relaunch a task agent, tail `~/.agman/agman.log`, expect 1–2 cycles of "session ready" without a delivery, then a clean deliver on the third cycle.

## Status

### Iteration 1 (this pass)

Implemented exactly as specified in the brief — all five change items landed in `src/tui/app.rs`.

**Reordering rationale.** The brief moved the readiness gate ahead of the already-pasted rescue. Old order let rescue run while the session was "not ready," which is the bug: a paste that landed during cold-start mounting could trigger rescue → Enter before Ink's input handler was alive. New order ensures rescue only runs against a session that's been ready for at least 3 seconds.

**Buffer accounting.** The 3s window is gated inside the `Ok((true, _))` readiness arm via `Instant::now()` recorded on first sighting and an `elapsed() < 3s` early-return. During the buffer window we push an empty `InboxPollResult` and `continue` — the `stuck_skip_counts.remove(&target)` happens *after* the buffer check, so a session that just turned ready stays counted as "fresh" rather than being instantly cleared. This matches the spec snippet.

**Re-arm on restart.** `Ok((false, _))` now does `first_ready_at.remove(&target)` so a kill+relaunch (which causes a transient false readiness) starts a fresh 3s window when claude comes back up.

**Concerns:** none. The change is mechanical and well-localized. `Instant`/`Duration` were already imported. The fallback `InboxPollOutput` in the `JoinError` path was updated for completeness so the `first_ready_at` map doesn't get silently wiped if the blocking task panics.

**No tests added** — the brief explicitly excludes this. The poller is tmux+time bound and isn't extracted today; extracting it just to test a 3-line buffer isn't worth the surface area.

**Next iteration focus.** Nothing actionable inside the coder↔checker loop. Verification is manual and runs outside this loop. Checker can hand off.
