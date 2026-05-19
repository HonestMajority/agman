mod helpers;

use agman::agent::Agent;
use agman::agent_model::{validate_agent_attachment, AgentAttachment, AgentKind, AgentRecord};
use agman::harness::{Harness, HarnessKind};
use helpers::{create_test_task, test_config};

fn harness() -> Box<dyn Harness> {
    HarnessKind::Claude.select()
}

#[test]
fn engineer_requires_task_attachment() {
    let unattached = validate_agent_attachment(&AgentKind::Engineer, &AgentAttachment::Unattached);
    assert!(unattached.is_err());

    validate_agent_attachment(
        &AgentKind::Engineer,
        &AgentAttachment::Task {
            task_id: "repo--branch".to_string(),
            role_label: Some("Engineer".to_string()),
        },
    )
    .unwrap();
}

#[test]
fn agent_model_serializes_engineer_attachment() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    let agent = AgentRecord::create_with_attachment(
        &config,
        "project",
        "engineer-repo-branch",
        "Build the thing",
        AgentKind::Engineer,
        AgentAttachment::Task {
            task_id: "repo--branch".to_string(),
            role_label: None,
        },
    )
    .unwrap();

    assert!(agent.is_engineer());
    assert!(agent.dir.starts_with(config.agents_dir()));
    let raw = std::fs::read_to_string(agent.dir.join("meta.json")).unwrap();
    assert!(raw.contains("\"type\": \"engineer\""));
    assert!(raw.contains("\"task_id\": \"repo--branch\""));

    let loaded = AgentRecord::load(agent.dir).unwrap();
    assert!(matches!(loaded.meta.kind, AgentKind::Engineer));
    assert!(matches!(
        loaded.meta.attachment,
        AgentAttachment::Task { ref task_id, .. } if task_id == "repo--branch"
    ));
}

#[test]
fn agent_prompt_uses_inbox_directives_without_sentinel_protocol() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();
    let task = create_test_task(&config, "repo", "branch");

    let agent = Agent::load(&config, "engineer").unwrap();
    let prompt = agent
        .build_system_prompt(&task, false, harness().as_ref())
        .unwrap();

    assert!(prompt.contains("Work Directives"));
    assert!(prompt.contains("[Message from repo]"));
    assert!(prompt.contains("agman send-message"));
    assert!(!prompt.contains("touch "));
}
