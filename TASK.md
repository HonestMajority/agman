# Goal
the navigation is a bit weird in the view when looking in a task. I would like to be able to switch back and forth between the
logs pane and the notes pane with h and l. currently we navigate backwards to the main view on h. we can remove that.

# Plan
## Completed
- [x] In `handle_preview_event` (src/tui/app.rs:987), modify the `h` key handler to switch to Logs pane instead of navigating back to TaskList
- [x] `l` key handler switches to Notes pane (already implemented at line 1047)
- [x] Bidirectional switching: `h` moves to Logs pane, `l` moves to Notes pane
- [x] `q` and `Esc` are now the only ways to go back to TaskList view
- [x] Build verified - compiles successfully

## Remaining
(none)
