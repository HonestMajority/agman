mod helpers;

use agman::agent::Agent;
use helpers::{create_test_task, test_config};

#[test]
fn agent_load() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let agent = Agent::load(&config, "planner").unwrap();
    assert_eq!(agent.name, "planner");
    assert!(!agent.prompt_template.is_empty());
}

#[test]
fn agent_build_prompt_basic() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let task = create_test_task(&config, "repo", "branch");
    task.write_task("# Goal\nBuild the widget\n\n# Plan\n- [ ] Step 1\n")
        .unwrap();

    let agent = Agent::load(&config, "planner").unwrap();
    let prompt = agent.build_prompt(&task).unwrap();

    // Should contain the template and the task content
    assert!(prompt.contains("planning agent"));
    assert!(prompt.contains("Build the widget"));
}

#[test]
fn agent_build_prompt_with_feedback() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let task = create_test_task(&config, "repo", "branch");
    task.write_task("# Goal\nOriginal goal\n").unwrap();
    task.write_feedback("Please fix the bug in main.rs").unwrap();

    let agent = Agent::load(&config, "refiner").unwrap();
    let prompt = agent.build_prompt(&task).unwrap();

    // Should contain the feedback section
    assert!(prompt.contains("Follow-up Feedback"));
    assert!(prompt.contains("Please fix the bug in main.rs"));
}

#[test]
fn agent_build_prompt_includes_self_improve_footer() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let task = create_test_task(&config, "repo", "branch");
    task.write_task("# Goal\nBuild something\n").unwrap();

    let agent = Agent::load(&config, "coder").unwrap();
    let prompt = agent.build_prompt(&task).unwrap();

    assert!(prompt.contains("# Self-Improvement"));
    assert!(prompt.contains("self-improve"));
    assert!(prompt.contains("Before outputting your final stop condition"));
}
