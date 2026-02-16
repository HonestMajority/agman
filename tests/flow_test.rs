mod helpers;

use agman::flow::{Flow, FlowStep, StopCondition};
use helpers::test_config;

#[test]
fn stop_condition_from_output() {
    assert_eq!(
        StopCondition::from_output("AGENT_DONE"),
        Some(StopCondition::AgentDone)
    );
    assert_eq!(
        StopCondition::from_output("TASK_COMPLETE"),
        Some(StopCondition::TaskComplete)
    );
    assert_eq!(
        StopCondition::from_output("TASK_BLOCKED"),
        None
    );
    assert_eq!(
        StopCondition::from_output("INPUT_NEEDED"),
        Some(StopCondition::InputNeeded)
    );
    assert_eq!(StopCondition::from_output("just some text"), None);
}

#[test]
fn stop_condition_embedded_in_text() {
    assert_eq!(
        StopCondition::from_output("The agent says AGENT_DONE here"),
        Some(StopCondition::AgentDone)
    );
}

#[test]
fn flow_load_default() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let flow = Flow::load(&config.flow_path("new")).unwrap();
    assert_eq!(flow.name, "new");
    assert_eq!(flow.steps.len(), 3); // prompt-builder, planner, loop

    // First step is prompt-builder
    match &flow.steps[0] {
        FlowStep::Agent(s) => assert_eq!(s.agent, "prompt-builder"),
        _ => panic!("expected Agent step"),
    }
    // Second step is planner
    match &flow.steps[1] {
        FlowStep::Agent(s) => assert_eq!(s.agent, "planner"),
        _ => panic!("expected Agent step"),
    }
    // Third step is a loop
    match &flow.steps[2] {
        FlowStep::Loop(l) => {
            assert_eq!(l.steps.len(), 2);
            assert_eq!(l.steps[0].agent, "coder");
            assert_eq!(l.steps[1].agent, "checker");
        }
        _ => panic!("expected Loop step"),
    }
}

#[test]
fn flow_load_continue() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let flow = Flow::load(&config.flow_path("continue")).unwrap();
    assert_eq!(flow.name, "continue");
    assert_eq!(flow.steps.len(), 2); // refiner, loop

    match &flow.steps[0] {
        FlowStep::Agent(s) => assert_eq!(s.agent, "refiner"),
        _ => panic!("expected Agent step"),
    }
    match &flow.steps[1] {
        FlowStep::Loop(l) => {
            assert_eq!(l.steps[0].agent, "coder");
            assert_eq!(l.steps[1].agent, "checker");
        }
        _ => panic!("expected Loop step"),
    }
}

#[test]
fn flow_load_new_multi() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let flow = Flow::load(&config.flow_path("new-multi")).unwrap();
    assert_eq!(flow.name, "new-multi");
    assert_eq!(flow.steps.len(), 4); // repo-inspector, prompt-builder, planner, loop

    // First step is repo-inspector with post_hook
    match &flow.steps[0] {
        FlowStep::Agent(s) => {
            assert_eq!(s.agent, "repo-inspector");
            assert_eq!(s.post_hook, Some("setup_repos".to_string()));
        }
        _ => panic!("expected Agent step"),
    }
    // Second step is prompt-builder
    match &flow.steps[1] {
        FlowStep::Agent(s) => assert_eq!(s.agent, "prompt-builder"),
        _ => panic!("expected Agent step"),
    }
    // Third step is planner
    match &flow.steps[2] {
        FlowStep::Agent(s) => assert_eq!(s.agent, "planner"),
        _ => panic!("expected Agent step"),
    }
    // Fourth step is a loop
    match &flow.steps[3] {
        FlowStep::Loop(l) => {
            assert_eq!(l.steps.len(), 2);
            assert_eq!(l.steps[0].agent, "coder");
            assert_eq!(l.steps[1].agent, "checker");
        }
        _ => panic!("expected Loop step"),
    }
}

#[test]
fn flow_get_step() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let flow = Flow::load(&config.flow_path("new")).unwrap();
    assert!(flow.get_step(0).is_some());
    assert!(flow.get_step(999).is_none());
}
