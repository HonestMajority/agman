mod helpers;

use agman::agent_model::{AgentAttachment, AgentKind, AgentRecord};
use agman::inbox;
use agman::repo_stats::RepoStats;
use agman::use_cases::{self, WorktreeSource};
use helpers::{
    create_test_project, create_test_researcher, create_test_task, init_test_repo, test_config,
};

#[test]
fn create_task_creates_one_attached_engineer_and_initial_inbox_message() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo = init_test_repo(&tmp, "repo");

    let task = use_cases::create_task(
        &config,
        "repo",
        "feature",
        "Build the widget",
        "engineer",
        WorktreeSource::NewBranch { base_branch: None },
        None,
        None,
    )
    .unwrap();

    assert!(task.dir.join("meta.json").exists());
    assert!(task.meta.primary_repo().worktree_path.exists());
    assert_eq!(
        RepoStats::load(&config.repo_stats_path())
            .counts
            .get("repo"),
        Some(&1)
    );

    let agents = use_cases::attached_agents_for_task(&config, "repo--feature").unwrap();
    assert_eq!(agents.len(), 1);
    assert!(agents[0].is_engineer());
    assert!(matches!(
        &agents[0].meta.attachment,
        AgentAttachment::Task { task_id, .. } if task_id == "repo--feature"
    ));

    let messages = inbox::read_messages(&config.agent_inbox("repo", &agents[0].meta.name)).unwrap();
    assert_eq!(messages.len(), 1);
    assert!(messages[0].message.contains("Build the widget"));
}

#[test]
fn empty_description_task_still_creates_attached_engineer() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo = init_test_repo(&tmp, "repo");

    use_cases::create_task(
        &config,
        "repo",
        "empty-desc",
        "",
        "new",
        WorktreeSource::NewBranch { base_branch: None },
        None,
        None,
    )
    .unwrap();

    let agents = use_cases::attached_agents_for_task(&config, "repo--empty-desc").unwrap();
    assert_eq!(agents.len(), 1);
    assert!(agents[0].is_engineer());
}

#[test]
fn send_message_targets_specific_attached_engineer() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _task = create_test_task(&config, "repo", "branch");
    let engineer = use_cases::attached_engineer_for_task(&config, "repo--branch").unwrap();

    use_cases::send_message(
        &config,
        &format!("engineer:repo--{}", engineer.meta.name),
        "repo",
        "Please tighten the tests",
    )
    .unwrap();

    let messages = inbox::read_messages(&config.agent_inbox("repo", &engineer.meta.name)).unwrap();
    assert!(messages
        .iter()
        .any(|message| message.message.contains("Please tighten the tests")));

    let task_target_err = use_cases::send_message(&config, "task:repo--branch", "repo", "nope")
        .unwrap_err()
        .to_string();
    assert!(task_target_err.contains("unknown target"));
}

#[test]
fn attach_detach_and_move_non_engineer_agents_preserve_single_engineer() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _task = create_test_task(&config, "repo", "branch");
    let _other_task = create_test_task(&config, "repo", "other");
    let _researcher = create_test_researcher(&config, "repo", "research");

    let unattached = use_cases::unattached_agents_for_project(&config, "repo").unwrap();
    assert_eq!(unattached.len(), 1);

    let attached = use_cases::attach_agent_to_task(
        &config,
        "repo",
        "research",
        "repo--branch",
        Some("Domain research".to_string()),
    )
    .unwrap();
    assert!(matches!(
        attached.meta.attachment,
        AgentAttachment::Task { ref task_id, .. } if task_id == "repo--branch"
    ));

    let moved =
        use_cases::move_agent_to_task(&config, "repo", "research", "repo--other", None).unwrap();
    assert!(matches!(
        moved.meta.attachment,
        AgentAttachment::Task { ref task_id, .. } if task_id == "repo--other"
    ));

    let detached = use_cases::detach_agent_from_task(&config, "repo", "research").unwrap();
    assert!(matches!(
        detached.meta.attachment,
        AgentAttachment::Unattached
    ));

    for task_id in ["repo--branch", "repo--other"] {
        let agents = use_cases::attached_agents_for_task(&config, task_id).unwrap();
        assert_eq!(agents.iter().filter(|agent| agent.is_engineer()).count(), 1);
    }
}

#[test]
fn engineer_cannot_be_manually_detached_or_moved() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _task = create_test_task(&config, "repo", "branch");
    let engineer = use_cases::attached_engineer_for_task(&config, "repo--branch").unwrap();

    let detach_err = use_cases::detach_agent_from_task(&config, "repo", &engineer.meta.name)
        .unwrap_err()
        .to_string();
    assert!(detach_err.contains("must remain attached"));

    let move_err =
        use_cases::move_agent_to_task(&config, "repo", &engineer.meta.name, "repo--branch", None)
            .unwrap_err()
            .to_string();
    assert!(move_err.contains("cannot be manually attached"));
}

#[test]
fn project_status_separates_attached_and_unattached_agents() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _project = create_test_project(&config, "repo");
    let _task = create_test_task(&config, "repo", "branch");
    let _researcher = create_test_researcher(&config, "repo", "research");

    let status = use_cases::aggregated_status(&config).unwrap();
    let repo = status
        .projects
        .iter()
        .find(|group| group.name == "repo")
        .unwrap();
    assert_eq!(repo.tasks.len(), 1);
    assert_eq!(
        repo.tasks[0].engineer.as_deref(),
        Some("engineer-repo-branch")
    );
    assert_eq!(repo.agents.len(), 1);
    assert_eq!(repo.agents[0].name, "research");
}

#[test]
fn prompts_describe_inbox_based_task_agent_model() {
    let chief = use_cases::build_chief_of_staff_prompt(false);
    assert!(chief.contains("agman send-message <target> --from chief-of-staff"));
    assert!(chief.contains("agman create-agent --kind <researcher|operator|reviewer|tester>"));
    assert!(chief.contains("agman attach-agent --project <project> --name <name> --task <task-id>"));
    assert!(!chief.contains("agman message <task-id>"));

    let pm = use_cases::build_pm_prompt(false, "project");
    assert!(pm.contains("Every task owns one attached Engineer agent"));
    assert!(pm.contains("task-attached Researcher, Tester, Reviewer, and Operator agents"));
    assert!(pm.contains("messaging the task's attached Engineer through the inbox"));
    assert!(pm.contains(
        "agman create-agent --kind <researcher|operator|reviewer|tester> --name <name> --project project"
    ));
    assert!(pm.contains("agman attach-agent --project project --name <name> --task <task-id>"));
    assert!(pm.contains("agman move-agent --project project --name <name> --task <task-id>"));
    assert!(pm.contains("agman detach-agent --project project --name <name>"));
    assert!(pm.contains("agman link-pr <task-id> <PR URL or number>"));
    assert!(pm.contains("PR URLs in inbox messages alone do not update task metadata"));
    assert!(!pm.contains("agman create-agent project <name>"));
    assert!(pm.contains("send-message"));

    let engineer =
        use_cases::build_engineer_prompt(false, "project", "engineer-repo-branch", "repo--branch");
    assert!(engineer.contains("attached to task \"repo--branch\""));
    assert!(engineer.contains("push branches"));
    assert!(engineer.contains("create or update pull requests"));
    assert!(engineer.contains("monitor CI"));
    assert!(engineer.contains("agman send-message project"));
    assert!(engineer.contains("agman link-pr repo--branch <PR URL or number>"));
    assert!(engineer.contains("Inbox messages alone do not link PRs into the agman TUI"));
    assert!(engineer.contains("send-message"));
}

#[test]
fn parse_pr_reference_accepts_numbers_and_github_urls() {
    assert_eq!(
        use_cases::parse_pr_reference("42").unwrap(),
        use_cases::PrReference::Number(42)
    );
    assert_eq!(
        use_cases::parse_pr_reference("https://github.com/acme/repo/pull/42/").unwrap(),
        use_cases::PrReference::Url {
            number: 42,
            url: "https://github.com/acme/repo/pull/42".to_string()
        }
    );

    assert!(use_cases::parse_pr_reference("0").is_err());
    assert!(use_cases::parse_pr_reference("https://example.com/acme/repo/pull/42").is_err());
    assert!(use_cases::parse_pr_reference("https://github.com/acme/repo/issues/42").is_err());
}

#[test]
fn link_task_pr_url_writes_linked_pr_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _task = create_test_task(&config, "repo", "branch");

    let linked = use_cases::link_task_pr(
        &config,
        "repo--branch",
        "https://github.com/acme/repo/pull/42",
        true,
        Some("alice".to_string()),
        false,
    )
    .unwrap();

    assert_eq!(linked.number, 42);
    assert_eq!(linked.url, "https://github.com/acme/repo/pull/42");
    assert!(linked.owned);
    assert_eq!(linked.author.as_deref(), Some("alice"));

    let task = agman::task::Task::load_by_id(&config, "repo--branch").unwrap();
    assert_eq!(task.meta.linked_pr.unwrap().number, 42);
}

#[test]
fn link_task_pr_number_builds_url_from_task_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let repo = init_test_repo(&tmp, "repo");
    std::process::Command::new("git")
        .args(["remote", "add", "origin", "git@github.com:acme/repo.git"])
        .current_dir(&repo)
        .status()
        .unwrap();
    let _task = use_cases::create_task(
        &config,
        "repo",
        "feature",
        "Build the widget",
        "engineer",
        WorktreeSource::NewBranch { base_branch: None },
        None,
        None,
    )
    .unwrap();

    let linked =
        use_cases::link_task_pr(&config, "repo--feature", "7", false, None, false).unwrap();

    assert_eq!(linked.number, 7);
    assert_eq!(linked.url, "https://github.com/acme/repo/pull/7");
    assert!(!linked.owned);
}

#[test]
fn link_task_pr_rejects_different_pr_without_force() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _task = create_test_task(&config, "repo", "branch");

    use_cases::link_task_pr(
        &config,
        "repo--branch",
        "https://github.com/acme/repo/pull/42",
        true,
        None,
        false,
    )
    .unwrap();
    use_cases::link_task_pr(
        &config,
        "repo--branch",
        "https://github.com/acme/repo/pull/42",
        false,
        Some("alice".to_string()),
        false,
    )
    .unwrap();

    let err = use_cases::link_task_pr(
        &config,
        "repo--branch",
        "https://github.com/acme/repo/pull/43",
        true,
        None,
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("already linked"));

    let linked = use_cases::link_task_pr(
        &config,
        "repo--branch",
        "https://github.com/acme/repo/pull/43",
        true,
        None,
        true,
    )
    .unwrap();
    assert_eq!(linked.number, 43);
}

#[test]
fn link_task_pr_from_sidecar_reads_legacy_pr_link() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let task = create_test_task(&config, "repo", "branch");
    std::fs::write(
        task.dir.join(".pr-link"),
        "42\nhttps://github.com/acme/repo/pull/42\n",
    )
    .unwrap();

    let linked =
        use_cases::link_task_pr_from_sidecar(&config, "repo--branch", true, None, false).unwrap();

    assert_eq!(linked.number, 42);
    assert_eq!(linked.url, "https://github.com/acme/repo/pull/42");
}

#[test]
fn task_with_missing_or_duplicate_engineer_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let task = create_test_task(&config, "repo", "branch");
    let engineer = use_cases::attached_engineer_for_task(&config, "repo--branch").unwrap();
    std::fs::remove_dir_all(&engineer.dir).unwrap();

    let missing = use_cases::attached_agents_for_task(&config, "repo--branch").unwrap_err();
    assert!(missing.to_string().contains("no attached engineer"));

    for name in ["one", "two"] {
        AgentRecord::create_with_attachment(
            &config,
            "repo",
            name,
            "extra engineer",
            AgentKind::Engineer,
            AgentAttachment::Task {
                task_id: task.meta.task_id(),
                role_label: Some("Engineer".to_string()),
            },
        )
        .unwrap();
    }
    let duplicate = use_cases::attached_agents_for_task(&config, "repo--branch").unwrap_err();
    assert!(duplicate.to_string().contains("attached engineers"));
}

#[test]
fn archive_task_archives_and_unlinks_attached_agents() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");
    let _researcher = create_test_researcher(&config, "repo", "research");
    use_cases::attach_agent_to_task(&config, "repo", "research", "repo--branch", None).unwrap();

    use_cases::archive_task(&config, &mut task, false).unwrap();

    assert!(task.meta.archived_at.is_some());
    for name in ["engineer-repo-branch", "research"] {
        let agent = AgentRecord::load(config.agent_dir("repo", name)).unwrap();
        assert_eq!(agent.meta.status, agman::agent_model::AgentStatus::Archived);
        assert!(matches!(agent.meta.attachment, AgentAttachment::Unattached));
    }
}

#[test]
fn permanently_delete_archived_task_archives_and_unlinks_stale_attached_agents() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");
    let _researcher = create_test_researcher(&config, "repo", "research");
    use_cases::attach_agent_to_task(&config, "repo", "research", "repo--branch", None).unwrap();
    task.meta.archived_at = Some(chrono::Utc::now());
    task.save_meta().unwrap();

    use_cases::permanently_delete_archived_task(&config, task).unwrap();

    assert!(!config.task_dir("repo", "branch").exists());
    for name in ["engineer-repo-branch", "research"] {
        let agent = AgentRecord::load(config.agent_dir("repo", name)).unwrap();
        assert_eq!(agent.meta.status, agman::agent_model::AgentStatus::Archived);
        assert!(matches!(agent.meta.attachment, AgentAttachment::Unattached));
    }
}

#[test]
fn fully_delete_task_archives_and_unlinks_attached_agents() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let task = create_test_task(&config, "repo", "branch");
    let _researcher = create_test_researcher(&config, "repo", "research");
    use_cases::attach_agent_to_task(&config, "repo", "research", "repo--branch", None).unwrap();

    use_cases::fully_delete_task(&config, task).unwrap();

    assert!(!config.task_dir("repo", "branch").exists());
    for name in ["engineer-repo-branch", "research"] {
        let agent = AgentRecord::load(config.agent_dir("repo", name)).unwrap();
        assert_eq!(agent.meta.status, agman::agent_model::AgentStatus::Archived);
        assert!(matches!(agent.meta.attachment, AgentAttachment::Unattached));
    }
}
