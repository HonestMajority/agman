mod helpers;

use agman::agent_model::{
    AgentAttachment, AgentKind, AgentRecord, AgentStatus, TesterCapabilities,
};
use agman::harness::HarnessKind;
use agman::inbox;
use agman::repo_stats::RepoStats;
use agman::use_cases::{self, WorktreeSource};
use chrono::{Duration, Utc};
use helpers::{
    create_test_agent, create_test_project, create_test_researcher, create_test_task,
    init_test_repo, test_config,
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
        Some("Build the widget"),
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
    assert!(messages[0]
        .message
        .contains("First prompt for repo--feature:"));
    assert!(messages[0].message.contains("Build the widget"));
    assert!(!messages[0].message.contains("Task goal for"));
}

#[test]
fn visible_fresh_inbox_message_is_deferred() {
    let now = Utc::now();
    let msg = inbox::InboxMessage {
        seq: 1,
        from: "pm".to_string(),
        message: "fresh".to_string(),
        timestamp: now - Duration::seconds(60),
    };

    assert!(use_cases::should_defer_visible_fresh_inbox_message(
        &msg,
        now,
        Some(true)
    ));
}

#[test]
fn hidden_fresh_inbox_message_is_not_deferred() {
    let now = Utc::now();
    let msg = inbox::InboxMessage {
        seq: 1,
        from: "pm".to_string(),
        message: "fresh".to_string(),
        timestamp: now - Duration::seconds(60),
    };

    assert!(!use_cases::should_defer_visible_fresh_inbox_message(
        &msg,
        now,
        Some(false)
    ));
}

#[test]
fn visibility_error_defers_fresh_but_not_old_inbox_message() {
    let now = Utc::now();
    let fresh = inbox::InboxMessage {
        seq: 1,
        from: "pm".to_string(),
        message: "fresh".to_string(),
        timestamp: now - Duration::seconds(60),
    };
    let old = inbox::InboxMessage {
        seq: 2,
        from: "pm".to_string(),
        message: "old".to_string(),
        timestamp: now - Duration::seconds(use_cases::INBOX_VISIBLE_FRESH_DEFERRAL_SECS),
    };

    assert!(use_cases::should_defer_visible_fresh_inbox_message(
        &fresh, now, None
    ));
    assert!(!use_cases::should_defer_visible_fresh_inbox_message(
        &old, now, None
    ));
}

#[test]
fn snippet_in_capture_matches_exact_and_wrapped_captures() {
    let snippet = "[msg:reviewer:agman--code-review:42]";

    // Exact, unwrapped occurrence.
    let capture = format!("some prompt\n{snippet} [Message from reviewer]: hi\n");
    assert!(use_cases::snippet_in_capture(&capture, snippet));

    // Snippet split across a hard line break (pane wrap at arbitrary column).
    let wrapped = "junk before [msg:reviewer:agman--c\node-review:42] [Message from...";
    assert!(use_cases::snippet_in_capture(wrapped, snippet));

    // Claude's renderer hard-wraps at its layout width and indents the
    // continuation line with a leading margin; tmux -J cannot join those.
    let claude_wrapped = "junk before [msg:reviewer:agman--code-rev\n  iew:42] [Message from...";
    assert!(use_cases::snippet_in_capture(claude_wrapped, snippet));

    // CRLF capture.
    let crlf = "line one\r\n[msg:reviewer:agman--code-review:4\r\n2] tail\r\n";
    assert!(use_cases::snippet_in_capture(crlf, snippet));

    // Absent snippet and wrong seq must not match.
    assert!(!use_cases::snippet_in_capture("nothing here\n", snippet));
    assert!(!use_cases::snippet_in_capture(
        "[msg:reviewer:agman--code-review:43]",
        snippet
    ));
    assert!(!use_cases::snippet_in_capture("", snippet));
}

#[test]
fn record_inbox_verify_failure_hits_threshold_after_consecutive_cycles() {
    let mut counts = std::collections::HashMap::new();

    assert!(!use_cases::record_inbox_verify_failure(
        &mut counts,
        "project-a",
        7,
        3
    ));
    assert!(!use_cases::record_inbox_verify_failure(
        &mut counts,
        "project-a",
        7,
        3
    ));
    // Third consecutive failed cycle for the same head seq crosses the threshold.
    assert!(use_cases::record_inbox_verify_failure(
        &mut counts,
        "project-a",
        7,
        3
    ));
    assert_eq!(counts.get("project-a"), Some(&(7, 3)));
}

#[test]
fn record_inbox_verify_failure_resets_streak_on_head_seq_change() {
    let mut counts = std::collections::HashMap::new();

    assert!(!use_cases::record_inbox_verify_failure(
        &mut counts,
        "project-a",
        7,
        3
    ));
    assert!(!use_cases::record_inbox_verify_failure(
        &mut counts,
        "project-a",
        7,
        3
    ));
    // Head seq advanced (e.g. delivered out-of-band): streak restarts at 1.
    assert!(!use_cases::record_inbox_verify_failure(
        &mut counts,
        "project-a",
        8,
        3
    ));
    assert_eq!(counts.get("project-a"), Some(&(8, 1)));
    assert!(!use_cases::record_inbox_verify_failure(
        &mut counts,
        "project-a",
        8,
        3
    ));
    assert!(use_cases::record_inbox_verify_failure(
        &mut counts,
        "project-a",
        8,
        3
    ));
}

#[test]
fn record_inbox_verify_failure_tracks_targets_independently() {
    let mut counts = std::collections::HashMap::new();

    assert!(!use_cases::record_inbox_verify_failure(
        &mut counts,
        "project-a",
        7,
        3
    ));
    assert!(!use_cases::record_inbox_verify_failure(
        &mut counts,
        "project-b",
        7,
        3
    ));
    assert!(!use_cases::record_inbox_verify_failure(
        &mut counts,
        "project-a",
        7,
        3
    ));
    assert_eq!(counts.get("project-a"), Some(&(7, 2)));
    assert_eq!(counts.get("project-b"), Some(&(7, 1)));

    // Successful delivery clears the target entry; a later failure starts a
    // fresh streak (mirrors the poll worker's remove-on-success).
    counts.remove("project-a");
    assert!(!use_cases::record_inbox_verify_failure(
        &mut counts,
        "project-a",
        7,
        3
    ));
    assert_eq!(counts.get("project-a"), Some(&(7, 1)));
}

#[test]
fn record_inbox_verify_failure_stays_at_or_above_threshold_until_cleared() {
    let mut counts = std::collections::HashMap::new();

    for _ in 0..3 {
        use_cases::record_inbox_verify_failure(&mut counts, "project-a", 7, 3);
    }
    // If the force-advance mark_delivered write fails, the next cycle must
    // still report threshold-reached so the force-advance is retried.
    assert!(use_cases::record_inbox_verify_failure(
        &mut counts,
        "project-a",
        7,
        3
    ));
}

#[test]
fn no_first_prompt_task_still_creates_attached_engineer_with_empty_inbox() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo = init_test_repo(&tmp, "repo");

    use_cases::create_task(
        &config,
        "repo",
        "idle",
        None,
        "new",
        WorktreeSource::NewBranch { base_branch: None },
        None,
        None,
    )
    .unwrap();

    let agents = use_cases::attached_agents_for_task(&config, "repo--idle").unwrap();
    assert_eq!(agents.len(), 1);
    assert!(agents[0].is_engineer());

    let messages = inbox::read_messages(&config.agent_inbox("repo", &agents[0].meta.name)).unwrap();
    assert!(messages.is_empty());
}

#[test]
fn blank_first_prompt_task_still_creates_attached_engineer_with_empty_inbox() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo = init_test_repo(&tmp, "repo");

    use_cases::create_task(
        &config,
        "repo",
        "blank-prompt",
        Some("  \n  "),
        "new",
        WorktreeSource::NewBranch { base_branch: None },
        None,
        None,
    )
    .unwrap();

    let agents = use_cases::attached_agents_for_task(&config, "repo--blank-prompt").unwrap();
    assert_eq!(agents.len(), 1);
    assert!(agents[0].is_engineer());

    let messages = inbox::read_messages(&config.agent_inbox("repo", &agents[0].meta.name)).unwrap();
    assert!(messages.is_empty());
}

#[test]
fn multi_repo_no_first_prompt_creates_idle_engineer_with_empty_inbox() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let parent_dir = tmp.path().join("repos");
    std::fs::create_dir_all(&parent_dir).unwrap();

    use_cases::create_multi_repo_task(
        &config,
        "repos",
        "multi-idle",
        None,
        "new-multi",
        parent_dir,
        None,
    )
    .unwrap();

    let agents = use_cases::attached_agents_for_task(&config, "repos--multi-idle").unwrap();
    assert_eq!(agents.len(), 1);
    assert!(agents[0].is_engineer());

    let messages =
        inbox::read_messages(&config.agent_inbox("repos", &agents[0].meta.name)).unwrap();
    assert!(messages.is_empty());
}

#[test]
fn create_researcher_with_first_prompt_seeds_one_inbox_message() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    create_test_project(&config, "project");

    let agent = use_cases::create_researcher(
        &config,
        "project",
        "researcher",
        Some("  Investigate the API latency  \n"),
        None,
        None,
        None,
    )
    .unwrap();

    assert_eq!(agent.meta.description, "Investigate the API latency");
    let messages = inbox::read_messages(&config.agent_inbox("project", "researcher")).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].from, "user");
    assert_eq!(messages[0].message, "Investigate the API latency");
}

#[test]
fn create_researcher_without_first_prompt_creates_idle_agent_with_empty_metadata_description() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    create_test_project(&config, "project");

    let agent =
        use_cases::create_researcher(&config, "project", "researcher", None, None, None, None)
            .unwrap();

    assert_eq!(agent.meta.description, "");
    let messages = inbox::read_messages(&config.agent_inbox("project", "researcher")).unwrap();
    assert!(messages.is_empty());
}

#[test]
fn create_researcher_with_blank_first_prompt_creates_idle_agent_with_empty_metadata_description() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    create_test_project(&config, "project");

    let agent = use_cases::create_researcher(
        &config,
        "project",
        "researcher",
        Some("  \n  "),
        None,
        None,
        None,
    )
    .unwrap();

    assert_eq!(agent.meta.description, "");
    let messages = inbox::read_messages(&config.agent_inbox("project", "researcher")).unwrap();
    assert!(messages.is_empty());
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
    assert!(chief.contains("agman create-pm-task <project> <repo> <task-name> [--first-prompt"));
    assert!(chief.contains("omitting the first prompt creates an idle Engineer"));
    assert!(chief.contains("agman create-agent --kind <researcher|operator|reviewer|tester>"));
    assert!(chief.contains("[--first-prompt"));
    assert!(chief.contains("creates an idle project-scoped agent"));
    assert!(chief.contains("agman attach-agent --project <project> --name <name> --task <task-id>"));
    assert!(!chief.contains("agman message <task-id>"));

    let pm = use_cases::build_pm_prompt(false, "project");
    assert!(pm.contains("Every task owns one attached Engineer agent"));
    assert!(pm.contains("task-attached Researcher, Tester, Reviewer, and Operator agents"));
    assert!(pm.contains("agman create-pm-task project <repo> <task-name> [--first-prompt"));
    assert!(pm.contains("messaging the task's attached Engineer through the inbox"));
    assert!(pm.contains(
        "agman create-agent --kind <researcher|operator|reviewer|tester> --name <name> --project project [--first-prompt"
    ));
    assert!(pm.contains("Omit it only when you intentionally want an idle agent"));
    assert!(pm.contains("agman attach-agent --project project --name <name> --task <task-id>"));
    assert!(pm.contains("agman move-agent --project project --name <name> --task <task-id>"));
    assert!(pm.contains("agman detach-agent --project project --name <name>"));
    assert!(pm.contains("agman link-pr <task-id> <PR URL or number>"));
    assert!(pm.contains("PR URLs in inbox messages alone do not update task metadata"));
    assert!(!pm.contains("agman create-agent project <name>"));
    assert!(!pm.contains("agman create-pm-task project <repo> <task-name> --description"));
    assert!(!pm.contains("agman create-agent --kind <researcher|operator|reviewer|tester> --name <name> --project project --description"));
    assert!(pm.contains("send-message"));

    let engineer =
        use_cases::build_engineer_prompt(false, "project", "engineer-repo-branch", "repo--branch");
    assert!(engineer.contains("attached to task \"repo--branch\""));
    assert!(engineer.contains("If no PM inbox message arrived for this task, wait"));
    assert!(engineer.contains("do not infer work from the task name, branch, or worktree alone"));
    assert!(engineer.contains("push branches"));
    assert!(engineer.contains("create or update pull requests"));
    assert!(engineer.contains("monitor CI"));
    assert!(engineer.contains("agman send-message project"));
    assert!(engineer.contains("agman link-pr repo--branch <PR URL or number>"));
    assert!(engineer.contains("Inbox messages alone do not link PRs into the agman TUI"));
    assert!(engineer.contains("send-message"));
}

#[test]
fn prompts_include_obsidian_operational_notes_guidance() {
    let chief = use_cases::build_chief_of_staff_prompt(false);
    assert_obsidian_common_base(&chief);
    assert_obsidian_cos_examples(&chief);
    assert!(chief.contains("When discussing a named project, list that project's folder too"));

    let project_prompts = [
        use_cases::build_pm_prompt(false, "project"),
        use_cases::build_engineer_prompt(false, "project", "engineer-repo-branch", "repo--branch"),
        use_cases::build_researcher_prompt(false, "project", "researcher"),
        use_cases::build_operator_prompt(false, "project", "operator"),
        use_cases::build_reviewer_prompt(false, "project", "reviewer", &[]),
        use_cases::build_tester_prompt(
            false,
            "project",
            "tester",
            &[],
            TesterCapabilities::default(),
            HarnessKind::Codex,
        ),
    ];

    for prompt in project_prompts {
        assert_obsidian_common_base(&prompt);
        assert_obsidian_project_examples(&prompt, "project");
    }
}

#[test]
fn reviewer_prompt_allows_only_concise_obsidian_notes_writes() {
    let reviewer = use_cases::build_reviewer_prompt(false, "project", "reviewer", &[]);

    assert!(reviewer
        .contains("Do **not** write to the reviewed worktrees or create local artifact files"));
    assert!(reviewer.contains(
        "Concise Obsidian operational notes are allowed only through the Obsidian guidance below"
    ));
    assert!(reviewer.contains("All roles, including reviewers, may write concise Obsidian notes"));
    assert_obsidian_project_examples(&reviewer, "project");
}

#[test]
fn telegram_guidance_stays_after_obsidian_notes_when_enabled() {
    let pm = use_cases::build_pm_prompt(true, "project");

    let obsidian_idx = pm.find("## Obsidian Operational Notes").unwrap();
    let telegram_idx = pm.find("## Telegram").unwrap();

    assert!(telegram_idx > obsidian_idx);
    assert!(pm[telegram_idx..].contains("IMMEDIATELY acknowledge"));
}

fn assert_obsidian_common_base(prompt: &str) {
    assert!(prompt.contains("vault=agman"));
    assert!(prompt.contains("obsidian vault=agman files folder=general"));
    assert!(prompt.contains("obsidian vault=agman read path=\"general/<note>.md\""));
    assert!(prompt.contains(
        "obsidian vault=agman search:context query=\"<keyword>\" path=general limit=5 format=json"
    ));
    assert!(prompt.contains("obsidian vault=agman create path="));
    assert!(prompt.contains("obsidian vault=agman append path="));
    assert!(prompt.contains("Do not create Obsidian project folders during agman project creation"));
    assert!(prompt.contains("must not make core agman project creation depend on Obsidian"));
    assert!(prompt.contains("no project notes yet"));
    assert!(prompt.contains("Project folders are created lazily on first durable note write"));
    assert!(prompt.contains("creates missing parent folders for `create path=...`"));
    assert!(prompt.contains("ask the PM/user to create the folder in Obsidian before retrying"));
    assert!(prompt.contains("Hard ban: no secrets, credentials, tokens"));
    assert!(prompt.contains("private sensitive data"));
    assert!(prompt.contains("system-prompt-style instructions"));
    assert!(prompt.contains(
        "current user/PM direction, repo state, live systems, CI, and agman task state override Obsidian notes"
    ));
    assert!(prompt.contains("do not read all notes"));
    assert!(prompt.contains("title directly matches"));
    assert!(prompt.contains("before creating or updating PR descriptions"));
    assert!(prompt.contains("general/Write PR description.md"));
}

fn assert_obsidian_cos_examples(prompt: &str) {
    assert!(prompt.contains("obsidian vault=agman files folder=\"projects/<project-name>\""));
    assert!(prompt.contains(
        "Treat an empty result from `obsidian vault=agman files folder=\"projects/<project-name>\"`"
    ));
    assert!(prompt.contains("obsidian vault=agman read path=\"projects/<project-name>/<note>.md\""));
    assert!(prompt.contains(
        "obsidian vault=agman search:context query=\"<keyword>\" path=\"projects/<project-name>\" limit=5 format=json"
    ));
    assert!(prompt.contains("obsidian vault=agman create path=\"general/<topic>.md\""));
    assert!(
        prompt.contains("obsidian vault=agman append path=\"projects/<project-name>/<topic>.md\"")
    );
    assert!(prompt.contains("updated: <YYYY-MM-DD>"));
    assert!(prompt.contains("last_verified: <YYYY-MM-DD>"));
}

fn assert_obsidian_project_examples(prompt: &str, project: &str) {
    assert!(!prompt.contains("projects/<project-name>"));
    assert!(prompt.contains(&format!(
        "obsidian vault=agman files folder=\"projects/{project}\""
    )));
    assert!(prompt.contains(&format!(
        "Treat an empty result from `obsidian vault=agman files folder=\"projects/{project}\"`"
    )));
    assert!(prompt.contains(&format!(
        "obsidian vault=agman read path=\"projects/{project}/<note>.md\""
    )));
    assert!(prompt.contains(&format!(
        "obsidian vault=agman search:context query=\"<keyword>\" path=\"projects/{project}\" limit=5 format=json"
    )));
    assert!(prompt.contains(&format!(
        "obsidian vault=agman create path=\"projects/{project}/<topic>.md\""
    )));
    assert!(prompt.contains(&format!(
        "obsidian vault=agman append path=\"projects/{project}/<topic>.md\""
    )));
    assert!(prompt.contains("updated: <YYYY-MM-DD>"));
    assert!(prompt.contains("last_verified: <YYYY-MM-DD>"));
    assert!(prompt.contains("Use `general/` only for genuinely reusable cross-project notes"));
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
        Some("Build the widget"),
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
fn link_task_pr_rejects_same_number_different_repo_without_force() {
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

    let err = use_cases::link_task_pr(
        &config,
        "repo--branch",
        "https://github.com/other/repo/pull/42",
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
        "https://github.com/other/repo/pull/42",
        true,
        None,
        true,
    )
    .unwrap();
    assert_eq!(linked.number, 42);
    assert_eq!(linked.url, "https://github.com/other/repo/pull/42");
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
fn project_notes_are_isolated_by_project_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    create_test_project(&config, "alpha");
    create_test_project(&config, "beta");
    let alpha_notes = config.project_notes_dir("alpha");
    let beta_notes = config.project_notes_dir("beta");
    std::fs::create_dir_all(&alpha_notes).unwrap();
    std::fs::create_dir_all(&beta_notes).unwrap();

    let alpha_note = use_cases::create_note(&alpha_notes, "plan").unwrap();
    let beta_note = use_cases::create_note(&beta_notes, "plan").unwrap();
    use_cases::save_note(&alpha_note, "alpha only").unwrap();
    use_cases::save_note(&beta_note, "beta only").unwrap();

    assert_eq!(use_cases::read_note(&alpha_note).unwrap(), "alpha only");
    assert_eq!(use_cases::read_note(&beta_note).unwrap(), "beta only");
    assert_eq!(
        alpha_note,
        config.project_dir("alpha").join("notes/plan.md")
    );
    assert_eq!(beta_note, config.project_dir("beta").join("notes/plan.md"));
}

#[test]
fn delete_project_removes_project_notes_with_project_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    create_test_project(&config, "notes-project");
    let notes_dir = config.project_notes_dir("notes-project");
    std::fs::create_dir_all(&notes_dir).unwrap();
    std::fs::write(notes_dir.join("scratch.md"), "remove me").unwrap();

    use_cases::delete_project(&config, "notes-project").unwrap();

    assert!(!config.project_dir("notes-project").exists());
    assert!(!notes_dir.exists());
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

#[test]
fn agent_switch_token_is_short_deterministic_hex() {
    let long_id =
        "researcher:citadel-partner-portal-deploy--drua-citadel-sales-portal-security-review";

    let token = use_cases::agent_switch_token(long_id);

    assert_eq!(token.len(), 10);
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(token, use_cases::agent_switch_token(long_id));
    assert_ne!(
        token,
        use_cases::agent_switch_token("researcher:citadel-partner-portal-deploy--other")
    );
}

#[test]
fn relative_agent_list_project_view_includes_running_agent_kinds_only() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _project = create_test_project(&config, "long-project-name");
    let _task = create_test_task(&config, "long-project-name", "long-task-branch");
    let _researcher = create_test_agent(
        &config,
        "long-project-name",
        "research-agent",
        AgentKind::Researcher {
            repo: None,
            branch: None,
            task_id: None,
        },
    );
    let _operator = create_test_agent(
        &config,
        "long-project-name",
        "operator-agent",
        AgentKind::Operator {
            repo: None,
            branch: None,
            task_id: None,
        },
    );
    let _reviewer = create_test_agent(
        &config,
        "long-project-name",
        "reviewer-agent",
        AgentKind::Reviewer { worktrees: vec![] },
    );
    let _tester = create_test_agent(
        &config,
        "long-project-name",
        "tester-agent",
        AgentKind::Tester {
            worktrees: vec![],
            capabilities: Default::default(),
        },
    );
    let mut archived = create_test_agent(
        &config,
        "long-project-name",
        "archived-agent",
        AgentKind::Researcher {
            repo: None,
            branch: None,
            task_id: None,
        },
    );
    archived.meta.status = AgentStatus::Archived;
    archived.save_meta().unwrap();

    let ids: Vec<String> = use_cases::relative_agent_list(&config, "long-project-name")
        .into_iter()
        .map(|agent| agent.id)
        .collect();

    assert!(ids.iter().any(|id| id
        .starts_with("engineer:long-project-name--engineer-long-project-name-long-task-branch")));
    assert!(ids.contains(&"researcher:long-project-name--research-agent".to_string()));
    assert!(ids.contains(&"operator:long-project-name--operator-agent".to_string()));
    assert!(ids.contains(&"reviewer:long-project-name--reviewer-agent".to_string()));
    assert!(ids.contains(&"tester:long-project-name--tester-agent".to_string()));
    assert!(ids.contains(&"chief-of-staff".to_string()));
    assert!(!ids.contains(&"researcher:long-project-name--archived-agent".to_string()));
}

#[test]
fn resolves_long_agent_switch_token_from_current_relative_list() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _project = create_test_project(&config, "citadel-partner-portal-deploy");
    let _researcher = create_test_agent(
        &config,
        "citadel-partner-portal-deploy",
        "drua-citadel-sales-portal-security-review",
        AgentKind::Researcher {
            repo: None,
            branch: None,
            task_id: None,
        },
    );
    let target =
        "researcher:citadel-partner-portal-deploy--drua-citadel-sales-portal-security-review";
    let callback_data = format!("sw:{}", use_cases::agent_switch_token(target));

    assert!(callback_data.len() <= 64);
    assert_eq!(
        use_cases::resolve_agent_switch_token(
            &config,
            "citadel-partner-portal-deploy",
            callback_data.strip_prefix("sw:").unwrap()
        ),
        Some(target.to_string())
    );
}

#[test]
fn stale_agent_switch_token_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _project = create_test_project(&config, "project");
    let stale_token = use_cases::agent_switch_token("researcher:project--deleted-agent");

    assert_eq!(
        use_cases::resolve_agent_switch_token(&config, "project", &stale_token),
        None
    );
}
