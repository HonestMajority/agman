# Goal
Make the `x` command (stored command list) behave consistently with `f` (feedback) and `t` (task editor). When pressed from the task list view, `x` should transition to the preview view first, then open the command list modal on top of the preview. When dismissed, the command list should return to the preview view (not the task list).

# Plan
## Completed
- [x] Changed h/l keys in preview view to switch between Logs/Notes panes
- [x] `q` and `Esc` are now the only ways to go back from preview to task list
- [x] Removed `KeyCode::Char('l')` from `handle_tasklist_event`
- [x] Removed "l preview" hint from status bar
- [x] In `handle_tasklist_event` (src/tui/app.rs:904-907), added `load_preview()` call before `open_command_list()`
- [x] In `draw_status_bar` (src/tui/ui.rs:693-708), added `x cmd` hint to the preview view status bar hints
- [x] In `handle_tasklist_event` (src/tui/app.rs:904-910), set `self.view = View::Preview` BEFORE calling `open_command_list()` so the preview view is properly activated
- [x] In `handle_command_list_event` (src/tui/app.rs:1423-1424), change the Esc/q handler to return to `View::Preview` instead of `View::TaskList`

## Remaining
(none)
