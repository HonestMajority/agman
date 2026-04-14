use agman::tmux::pane_shows_claude_ready;

#[test]
fn pane_ready_claude_ready_wide() {
    let content = include_str!("fixtures/pane_ready/claude_ready_wide.txt");
    assert!(pane_shows_claude_ready(content));
}

#[test]
fn pane_ready_claude_ready_narrow() {
    let content = include_str!("fixtures/pane_ready/claude_ready_narrow.txt");
    assert!(pane_shows_claude_ready(content));
}

#[test]
fn pane_ready_claude_loading() {
    let content = include_str!("fixtures/pane_ready/claude_loading.txt");
    assert!(!pane_shows_claude_ready(content));
}

#[test]
fn pane_ready_claude_trust_prompt() {
    let content = include_str!("fixtures/pane_ready/claude_trust_prompt.txt");
    assert!(!pane_shows_claude_ready(content));
}

#[test]
fn pane_ready_claude_resume_fail() {
    let content = include_str!("fixtures/pane_ready/claude_resume_fail.txt");
    assert!(!pane_shows_claude_ready(content));
}

#[test]
fn pane_ready_claude_thinking() {
    let content = include_str!("fixtures/pane_ready/claude_thinking.txt");
    assert!(pane_shows_claude_ready(content));
}

#[test]
fn pane_ready_empty() {
    let content = include_str!("fixtures/pane_ready/empty.txt");
    assert!(!pane_shows_claude_ready(content));
}
