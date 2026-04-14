use agman::tmux::{has_input_prompt, has_startup_modal};

// -- has_startup_modal tests --

#[test]
fn modal_present_in_first_15_lines() {
    let content = include_str!("fixtures/pane_ready/modal_present.txt");
    assert!(has_startup_modal(content));
}

#[test]
fn modal_absent_normal_ready_screen() {
    let content = include_str!("fixtures/pane_ready/modal_absent.txt");
    assert!(!has_startup_modal(content));
}

#[test]
fn modal_phrases_deep_in_scrollback_not_detected() {
    let content = include_str!("fixtures/pane_ready/modal_phrases_deep.txt");
    assert!(!has_startup_modal(content));
}

#[test]
fn modal_empty_content() {
    let content = include_str!("fixtures/pane_ready/empty.txt");
    assert!(!has_startup_modal(content));
}

// -- has_input_prompt tests --

#[test]
fn input_prompt_wide_ready() {
    let content = include_str!("fixtures/pane_ready/claude_ready_wide.txt");
    assert!(has_input_prompt(content));
}

#[test]
fn input_prompt_narrow_ready() {
    let content = include_str!("fixtures/pane_ready/claude_ready_narrow.txt");
    assert!(has_input_prompt(content));
}

#[test]
fn input_prompt_loading_not_ready() {
    let content = include_str!("fixtures/pane_ready/claude_loading.txt");
    assert!(!has_input_prompt(content));
}

#[test]
fn input_prompt_thinking() {
    let content = include_str!("fixtures/pane_ready/claude_thinking.txt");
    assert!(has_input_prompt(content));
}

#[test]
fn input_prompt_empty() {
    let content = include_str!("fixtures/pane_ready/empty.txt");
    assert!(!has_input_prompt(content));
}
