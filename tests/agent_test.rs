mod helpers;

use agman::agent::Agent;
use agman::harness::{Harness, HarnessKind};
use helpers::{create_test_task, test_config};

fn harness() -> Box<dyn Harness> {
    HarnessKind::Claude.select()
}

#[test]
fn agent_load() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let agent = Agent::load(&config, "coder").unwrap();
    assert_eq!(agent.name, "coder");
    assert!(!agent.prompt_template.is_empty());
}

// ---------------------------------------------------------------------------
// build_system_prompt — identity payload (no TASK.md / feedback / git)
// ---------------------------------------------------------------------------

#[test]
fn agent_build_system_prompt_basic() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let task = create_test_task(&config, "repo", "branch");
    task.write_task("# Goal\nBuild the widget\n\n# Plan\n- [ ] Step 1\n")
        .unwrap();

    let agent = Agent::load(&config, "coder").unwrap();
    let prompt = agent
        .build_system_prompt(&task, false, harness().as_ref())
        .unwrap();

    // Identity content from the prompt template is present
    assert!(prompt.contains("coding agent"));
    // Work-directives preamble explains where work arrives
    assert!(prompt.contains("Work Directives"));
    // No project assigned → sender falls back to "supervisor"
    assert!(prompt.contains("[Message from supervisor]"));
    // The system prompt must NOT contain the dynamic payload — TASK.md is
    // delivered through the inbox, not the system prompt.
    assert!(
        !prompt.contains("Build the widget"),
        "TASK.md content must not be embedded in the system prompt"
    );
    assert!(
        !prompt.contains("# Current Task"),
        "Current Task heading belongs in the inbox message, not the system prompt"
    );
}

#[test]
fn agent_build_system_prompt_omits_feedback_and_git_context() {
    // Even with feedback queued, the system prompt stays identity-only.
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let task = create_test_task(&config, "repo", "branch");
    task.write_task("# Goal\nOriginal goal\n").unwrap();
    task.write_feedback("Please fix the bug in main.rs")
        .unwrap();

    let agent = Agent::load(&config, "refiner").unwrap();
    let prompt = agent
        .build_system_prompt(&task, false, harness().as_ref())
        .unwrap();

    assert!(
        !prompt.contains("Follow-up Feedback"),
        "feedback belongs in the inbox message"
    );
    assert!(!prompt.contains("Please fix the bug in main.rs"));
}

#[test]
fn agent_build_system_prompt_includes_self_improve_footer() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let task = create_test_task(&config, "repo", "branch");
    task.write_task("# Goal\nBuild something\n").unwrap();

    let agent = Agent::load(&config, "coder").unwrap();
    let prompt = agent
        .build_system_prompt(&task, false, harness().as_ref())
        .unwrap();

    assert!(prompt.contains("# Self-Improvement"));
    assert!(prompt.contains("self-improve"));
    assert!(prompt.contains("Before completing"));
}

#[test]
fn agent_build_system_prompt_includes_skill_awareness_footer() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let task = create_test_task(&config, "repo", "branch");
    task.write_task("# Goal\nBuild something\n").unwrap();

    let agent = Agent::load(&config, "coder").unwrap();
    let prompt = agent
        .build_system_prompt(&task, false, harness().as_ref())
        .unwrap();

    assert!(prompt.contains("# Skills"));
    assert!(prompt.contains(".claude/skills/"));
    assert!(prompt.contains(".claude/commands/"));
}

#[test]
fn agent_build_system_prompt_includes_supervisor_sentinel_directive() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let task = create_test_task(&config, "repo", "branch");
    task.write_task("# Goal\nDo something\n").unwrap();

    let agent = Agent::load(&config, "coder").unwrap();
    let prompt = agent
        .build_system_prompt(&task, false, harness().as_ref())
        .unwrap();

    // The sentinel section must list all three sentinel files and reference
    // this task's directory so the supervisor can find them.
    assert!(prompt.contains("Supervisor Sentinel"));
    assert!(prompt.contains("touch"));
    assert!(prompt.contains(".agent-done"));
    assert!(prompt.contains(".task-complete"));
    assert!(prompt.contains(".input-needed"));
    assert!(prompt.contains(&task.dir.display().to_string()));
    // Magic strings are no longer used for detection.
    assert!(!prompt.contains("printing your stop condition"));
}

#[test]
fn agent_build_system_prompt_uses_project_as_inbox_sender_tag() {
    // When the task has a project assigned, the work-directives preamble
    // should tell the agent to look for `[Message from <project>]:` so the
    // sender tag matches what the inbox poller injects.
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let mut task = create_test_task(&config, "repo", "branch");
    task.meta.project = Some("agman-ceo-pm".to_string());
    task.save_meta().unwrap();
    task.write_task("# Goal\nDo something\n").unwrap();

    let agent = Agent::load(&config, "coder").unwrap();
    let prompt = agent
        .build_system_prompt(&task, false, harness().as_ref())
        .unwrap();
    assert!(prompt.contains("[Message from agman-ceo-pm]"));
    assert!(!prompt.contains("[Message from supervisor]"));
}

// ---------------------------------------------------------------------------
// build_inbox_message — dynamic per-launch work payload
// ---------------------------------------------------------------------------

#[test]
fn agent_build_inbox_message_includes_task_md() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let task = create_test_task(&config, "repo", "branch");
    task.write_task("# Goal\nBuild the widget\n\n# Plan\n- [ ] Step 1\n")
        .unwrap();

    let agent = Agent::load(&config, "coder").unwrap();
    let msg = agent.build_inbox_message(&task, false).unwrap();

    assert!(msg.contains("# Current Task"));
    assert!(msg.contains("Build the widget"));
    assert!(msg.contains("Step 1"));
}

#[test]
fn agent_build_inbox_message_includes_feedback_when_present() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let task = create_test_task(&config, "repo", "branch");
    task.write_task("# Goal\nOriginal goal\n").unwrap();
    task.write_feedback("Please fix the bug in main.rs")
        .unwrap();

    let agent = Agent::load(&config, "refiner").unwrap();
    let msg = agent.build_inbox_message(&task, false).unwrap();

    assert!(msg.contains("Follow-up Feedback"));
    assert!(msg.contains("Please fix the bug in main.rs"));
}

#[test]
fn agent_build_inbox_message_omits_feedback_section_when_empty() {
    // Without feedback, the coder agent's inbox message should not include
    // the "Follow-up Feedback" header.
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let task = create_test_task(&config, "repo", "branch");
    task.write_task("# Goal\nDo work\n").unwrap();

    let agent = Agent::load(&config, "coder").unwrap();
    let msg = agent.build_inbox_message(&task, false).unwrap();

    assert!(!msg.contains("Follow-up Feedback"));
}

// ---------------------------------------------------------------------------
// command_mode = true — stored-command agents (pr-creator, push-rebaser, …)
// ---------------------------------------------------------------------------

#[test]
fn agent_command_mode_omits_prompt_template_from_system_prompt() {
    // For stored-command agents, the prompt template (the "action") moves to
    // the inbox message. The system prompt must NOT contain it — system
    // prompts don't trigger work, only user messages do.
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let task = create_test_task(&config, "repo", "branch");
    task.write_task("# Goal\nReady to ship\n").unwrap();

    let agent = Agent::load(&config, "pr-creator").unwrap();
    let prompt = agent
        .build_system_prompt(&task, true, harness().as_ref())
        .unwrap();

    // Boilerplate is still present
    assert!(prompt.contains("## Work Directives"));
    assert!(prompt.contains("# Skills"));
    assert!(prompt.contains("# Self-Improvement"));
    assert!(prompt.contains("# Supervisor Sentinel"));
    assert!(prompt.contains(".agent-done"));

    // The action instructions must NOT be in the system prompt
    assert!(
        !prompt.contains("PR creation agent"),
        "command-mode system prompt must not embed the prompt template; got:\n{}",
        prompt
    );
    assert!(
        !prompt.contains("gh pr view"),
        "action instructions must move to the inbox message in command mode"
    );
}

#[test]
fn agent_command_mode_inbox_message_has_action_then_task_context() {
    // For stored-command agents, the prompt template IS the action directive.
    // The inbox message must include it as the primary content under
    // `# Action`, then TASK.md as `# Task context (for reference)`.
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let task = create_test_task(&config, "repo", "branch");
    task.write_task("# Goal\nShip the widget\n## Status\nDone\n")
        .unwrap();

    let agent = Agent::load(&config, "pr-creator").unwrap();
    let msg = agent.build_inbox_message(&task, true).unwrap();

    // Action header with the prompt template content
    assert!(msg.contains("# Action"), "missing # Action header");
    assert!(
        msg.contains("PR creation agent"),
        "prompt template should be in the inbox message under # Action"
    );
    assert!(
        msg.contains("gh pr view"),
        "action instructions should be in the inbox message"
    );

    // TASK.md is reference context, not a directive
    assert!(
        msg.contains("# Task context (for reference)"),
        "missing TASK.md context header"
    );
    assert!(msg.contains("Ship the widget"));

    // The action must come BEFORE the task context — agents act on the first
    // directive they see, and the action is the primary directive here.
    let action_idx = msg.find("# Action").expect("action header missing");
    let context_idx = msg
        .find("# Task context (for reference)")
        .expect("context header missing");
    assert!(
        action_idx < context_idx,
        "action must precede task context in command-mode inbox message"
    );
}

#[test]
fn agent_command_mode_system_prompt_describes_inbox_action_payload() {
    // Work directives in command mode should tell the agent the inbox
    // message contains the action — not just "TASK.md and feedback".
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let task = create_test_task(&config, "repo", "branch");
    task.write_task("# Goal\nGo\n").unwrap();

    let agent = Agent::load(&config, "pr-merge-agent").unwrap();
    let prompt = agent
        .build_system_prompt(&task, true, harness().as_ref())
        .unwrap();

    assert!(prompt.contains("## Work Directives"));
    // Wording must hint that the inbox message carries the action directive.
    assert!(
        prompt.contains("action"),
        "command-mode work directives should mention the action payload"
    );
}
