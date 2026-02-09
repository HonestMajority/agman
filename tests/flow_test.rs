mod helpers;

use agman::flow::{Flow, FlowAction, FlowExecutor, FlowStep, StopCondition};
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
        Some(StopCondition::TaskBlocked)
    );
    assert_eq!(
        StopCondition::from_output("TESTS_PASS"),
        Some(StopCondition::TestsPass)
    );
    assert_eq!(
        StopCondition::from_output("TESTS_FAIL"),
        Some(StopCondition::TestsFail)
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
fn flow_load_tdd() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let flow = Flow::load(&config.flow_path("tdd")).unwrap();
    assert_eq!(flow.name, "tdd");
    assert_eq!(flow.steps.len(), 2); // planner, loop

    match &flow.steps[1] {
        FlowStep::Loop(l) => {
            assert_eq!(l.steps.len(), 3);
            assert_eq!(l.steps[0].agent, "test-writer");
            assert_eq!(l.steps[1].agent, "coder");
            assert_eq!(l.steps[2].agent, "tester");
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
fn flow_get_step_and_is_complete() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let flow = Flow::load(&config.flow_path("new")).unwrap();
    assert!(flow.get_step(0).is_some());
    assert!(flow.get_step(999).is_none());
    assert!(flow.is_complete(flow.steps.len()));
    assert!(!flow.is_complete(0));
}

#[test]
fn flow_executor_handle_output() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    // Use the continue flow: [refiner (Agent), loop (Loop)]
    let flow = Flow::load(&config.flow_path("continue")).unwrap();
    let mut exec = FlowExecutor::new(flow, 0);

    // AgentDone on step 0 (Agent step) should advance
    let action = exec.handle_output("AGENT_DONE");
    assert_eq!(action, FlowAction::AdvanceStep);
    assert_eq!(exec.current_step, 1);

    // TaskComplete from any step should complete
    let mut exec2 = FlowExecutor::new(Flow::load(&config.flow_path("continue")).unwrap(), 0);
    let action = exec2.handle_output("TASK_COMPLETE");
    assert_eq!(action, FlowAction::Complete);

    // No magic string should return RunAgent
    let mut exec3 = FlowExecutor::new(Flow::load(&config.flow_path("continue")).unwrap(), 0);
    let action = exec3.handle_output("just some output");
    assert_eq!(action, FlowAction::RunAgent(0));

    // InputNeeded should pause
    let mut exec4 = FlowExecutor::new(Flow::load(&config.flow_path("continue")).unwrap(), 0);
    let action = exec4.handle_output("INPUT_NEEDED");
    assert_eq!(action, FlowAction::Pause);
}
