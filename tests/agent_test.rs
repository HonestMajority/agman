mod helpers;

use agman::agent::Agent;
use helpers::{create_test_task, test_config};

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
    let prompt = agent.build_system_prompt(&task).unwrap();

    // Identity content from the prompt template is present
    assert!(prompt.contains("coding agent"));
    // Work-directives preamble explains where work arrives
    assert!(prompt.contains("Work Directives"));
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
    task.write_feedback("Please fix the bug in main.rs").unwrap();

    let agent = Agent::load(&config, "refiner").unwrap();
    let prompt = agent.build_system_prompt(&task).unwrap();

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
    let prompt = agent.build_system_prompt(&task).unwrap();

    assert!(prompt.contains("# Self-Improvement"));
    assert!(prompt.contains("self-improve"));
    assert!(prompt.contains("Before outputting your final stop condition"));
}

#[test]
fn agent_build_system_prompt_includes_skill_awareness_footer() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let task = create_test_task(&config, "repo", "branch");
    task.write_task("# Goal\nBuild something\n").unwrap();

    let agent = Agent::load(&config, "coder").unwrap();
    let prompt = agent.build_system_prompt(&task).unwrap();

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
    let prompt = agent.build_system_prompt(&task).unwrap();

    // The sentinel path must reference this task's directory so the
    // supervisor's poll loop can find the .agent-done file.
    assert!(prompt.contains("Supervisor Sentinel"));
    assert!(prompt.contains(".agent-done"));
    assert!(prompt.contains(&task.dir.display().to_string()));
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
    let msg = agent.build_inbox_message(&task).unwrap();

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
    task.write_feedback("Please fix the bug in main.rs").unwrap();

    let agent = Agent::load(&config, "refiner").unwrap();
    let msg = agent.build_inbox_message(&task).unwrap();

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
    let msg = agent.build_inbox_message(&task).unwrap();

    assert!(!msg.contains("Follow-up Feedback"));
}
