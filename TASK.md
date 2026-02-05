# Goal
Remove the `l` key shortcut from the main task list view. Currently both `Enter` and `l` open the preview view, but only `Enter` should do this. The `l` key should have no action in the task list view.

# Plan
## Completed
- [x] Changed h/l keys in preview view to switch between Logs and Notes panes
- [x] `q` and `Esc` are now the only ways to go back from preview to task list
- [x] In `handle_tasklist_event` (src/tui/app.rs:867), remove `KeyCode::Char('l')` from the match arm that opens preview view - keep only `KeyCode::Enter`

## Remaining
(none)
