use agman::harness::{HarnessKind, LaunchContext, SessionKey};

fn cwd() -> std::path::PathBuf {
    std::env::temp_dir()
}

#[test]
fn harness_kind_round_trips_through_strings() {
    assert_eq!(HarnessKind::from_str("claude"), Some(HarnessKind::Claude));
    assert_eq!(HarnessKind::from_str("codex"), Some(HarnessKind::Codex));
    assert_eq!(HarnessKind::from_str("nope"), None);
    assert_eq!(HarnessKind::Claude.as_str(), "claude");
    assert_eq!(HarnessKind::Codex.as_str(), "codex");
    assert_eq!(HarnessKind::ALL, &[HarnessKind::Claude, HarnessKind::Codex]);
}

#[test]
fn claude_build_session_command_emits_system_prompt_and_name() {
    let h = HarnessKind::Claude.select();
    let cmd = h.build_session_command(&LaunchContext {
        identity: "Identity body",
        name: "agman-task-myrepo--feat-x-step-1",
        cwd: &cwd(),
        skip_git_repo_check: true,
        no_alt_screen: false,
        session_key: SessionKey::Auto,
    });
    assert!(cmd.starts_with("claude"));
    assert!(cmd.contains("--dangerously-skip-permissions"));
    assert!(cmd.contains("--system-prompt 'Identity body'"));
    assert!(cmd.contains("--name 'agman-task-myrepo--feat-x-step-1'"));
    assert!(!cmd.contains("--system-prompt-file"));
    assert!(!cmd.contains("--resume"));
    assert!(!cmd.contains("--session-id"));
}

#[test]
fn claude_build_session_command_escapes_inner_single_quotes() {
    let h = HarnessKind::Claude.select();
    let cmd = h.build_session_command(&LaunchContext {
        identity: "It's a body with 'quotes'",
        name: "agman-x",
        cwd: &cwd(),
        skip_git_repo_check: true,
        no_alt_screen: false,
        session_key: SessionKey::Auto,
    });
    // Single-quote escape in shell: ' becomes '\''
    assert!(cmd.contains("It'\\''s a body with '\\''quotes'\\''"));
}

#[test]
fn claude_build_session_command_pins_session_id_when_provided() {
    // Long-lived first launch: claude pins the supplied UUID via
    // --session-id so a later --resume <uuid> lands directly in
    // interactive mode (not the picker).
    let h = HarnessKind::Claude.select();
    let uuid = "11111111-2222-3333-4444-555555555555";
    let cmd = h.build_session_command(&LaunchContext {
        identity: "Identity body",
        name: "agman-ceo",
        cwd: &cwd(),
        skip_git_repo_check: true,
        no_alt_screen: false,
        session_key: SessionKey::Pin(uuid),
    });
    assert!(cmd.contains(&format!("--session-id '{uuid}'")));
    assert!(cmd.contains("--system-prompt 'Identity body'"));
    assert!(cmd.contains("--name 'agman-ceo'"));
    assert!(!cmd.contains("--resume"));
}

#[test]
fn claude_build_session_command_resumes_when_provided() {
    // Long-lived resume: --resume <uuid> AND no --system-prompt (the
    // saved thread keeps its original prompt).
    let h = HarnessKind::Claude.select();
    let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    let cmd = h.build_session_command(&LaunchContext {
        identity: "Identity body",
        name: "agman-ceo",
        cwd: &cwd(),
        skip_git_repo_check: true,
        no_alt_screen: false,
        session_key: SessionKey::Resume(uuid),
    });
    assert!(cmd.contains(&format!("--resume '{uuid}'")));
    assert!(cmd.contains("--name 'agman-ceo'"));
    assert!(
        !cmd.contains("--system-prompt"),
        "resume must NOT pass --system-prompt: {cmd}"
    );
    assert!(!cmd.contains("--session-id"));
}

#[test]
fn codex_build_session_command_emits_developer_instructions_and_no_alt_screen() {
    let h = HarnessKind::Codex.select();
    let cmd = h.build_session_command(&LaunchContext {
        identity: "Identity body",
        name: "agman-task-myrepo--feat-x-step-1",
        cwd: &cwd(),
        skip_git_repo_check: true,
        no_alt_screen: true,
        session_key: SessionKey::Auto,
    });
    assert!(cmd.starts_with("codex"));
    assert!(cmd.contains("--skip-git-repo-check"));
    assert!(cmd.contains("--no-alt-screen"));
    assert!(cmd.contains("developer_instructions=\"\"\"Identity body\"\"\""));
    // Codex doesn't take --name; the name is registered post-launch via /rename.
    assert!(!cmd.contains("--name"));
    assert!(!cmd.contains("--resume"));
    assert!(!cmd.contains(" resume "));
}

#[test]
fn codex_build_session_command_emits_resume_subcommand() {
    // Long-lived resume: `codex resume <name>` shape with -C <cwd> and
    // --no-alt-screen. Skips developer_instructions (the saved thread
    // keeps its original prompt) and the git-repo guard.
    let h = HarnessKind::Codex.select();
    let work_dir = cwd();
    let cmd = h.build_session_command(&LaunchContext {
        identity: "Identity body",
        name: "agman-ceo",
        cwd: &work_dir,
        skip_git_repo_check: true,
        no_alt_screen: true,
        session_key: SessionKey::Resume("agman-ceo"),
    });
    assert!(cmd.starts_with("codex"));
    assert!(cmd.contains(" resume 'agman-ceo'"));
    assert!(cmd.contains(&format!(" -C '{}'", work_dir.to_string_lossy())));
    assert!(cmd.contains("--no-alt-screen"));
    assert!(
        !cmd.contains("developer_instructions"),
        "resume must NOT pass developer_instructions: {cmd}"
    );
    assert!(
        !cmd.contains("--skip-git-repo-check"),
        "resume omits --skip-git-repo-check: {cmd}"
    );
}

#[test]
fn codex_build_session_command_escapes_triple_quotes_in_body() {
    // Defensive: a triple-quoted string in the identity body would close
    // the TOML triple-quoted literal. We escape literal """ to \"\"\".
    let h = HarnessKind::Codex.select();
    let cmd = h.build_session_command(&LaunchContext {
        identity: "Pre \"\"\" mid",
        name: "x",
        cwd: &cwd(),
        skip_git_repo_check: false,
        no_alt_screen: false,
        session_key: SessionKey::Auto,
    });
    assert!(cmd.contains("Pre \\\"\\\"\\\" mid"));
    assert!(!cmd.contains("Pre \"\"\" mid"));
}

#[test]
fn claude_skill_hint_mentions_dot_claude() {
    let h = HarnessKind::Claude.select();
    let hint = h.skill_hint();
    assert!(hint.contains(".claude/skills/"));
    assert!(hint.contains(".claude/commands/"));
}

#[test]
fn codex_skill_hint_is_empty() {
    let h = HarnessKind::Codex.select();
    assert_eq!(h.skill_hint(), "");
}

#[test]
fn install_hints_match_documented_text() {
    assert!(HarnessKind::Claude
        .select()
        .install_hint()
        .contains("@anthropic-ai/claude-code"));
    let codex_hint = HarnessKind::Codex.select().install_hint();
    assert!(codex_hint.contains("codex") && codex_hint.contains("brew"));
}

#[test]
fn cli_binaries_match_kinds() {
    assert_eq!(HarnessKind::Claude.select().cli_binary(), "claude");
    assert_eq!(HarnessKind::Codex.select().cli_binary(), "codex");
}

#[test]
fn claude_latest_transcript_picks_newest_jsonl_for_cwd() {
    let claude_home = tempfile::tempdir().unwrap();
    std::env::set_var("AGMAN_CLAUDE_HOME", claude_home.path());

    let cwd = tempfile::tempdir().unwrap();
    let escaped = cwd.path().to_string_lossy().replace('/', "-");
    let agent_dir = claude_home.path().join("projects").join(escaped);
    std::fs::create_dir_all(&agent_dir).unwrap();

    let older = agent_dir.join("a.jsonl");
    let newer = agent_dir.join("b.jsonl");
    std::fs::write(&older, "{}\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&newer, "{}\n").unwrap();

    let h = HarnessKind::Claude.select();
    let pick = h.latest_transcript(cwd.path()).unwrap();
    assert_eq!(pick, newer);

    std::env::remove_var("AGMAN_CLAUDE_HOME");
}

#[test]
fn claude_find_last_assistant_marker_returns_uuid() {
    let h = HarnessKind::Claude.select();
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("t.jsonl");
    std::fs::write(
        &p,
        "{\"type\":\"user\",\"uuid\":\"u1\"}\n{\"type\":\"assistant\",\"uuid\":\"a1\"}\n{\"type\":\"assistant\",\"uuid\":\"a2\"}\n",
    )
    .unwrap();
    assert_eq!(h.find_last_assistant_marker(&p), Some("a2".to_string()));
}

#[test]
fn codex_find_last_assistant_marker_returns_timestamp() {
    let h = HarnessKind::Codex.select();
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("rollout.jsonl");
    std::fs::write(
        &p,
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\"},\"timestamp\":\"2026-01-01T00:01:00Z\"}\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\"},\"timestamp\":\"2026-01-01T00:02:00Z\"}\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\"},\"timestamp\":\"2026-01-01T00:03:00Z\"}\n",
    )
    .unwrap();
    assert_eq!(
        h.find_last_assistant_marker(&p),
        Some("2026-01-01T00:03:00Z".to_string())
    );
}

#[test]
fn codex_session_index_walker_matches_thread_name() {
    // Regression: codex writes `thread_name` (not `name`) in
    // session_index.jsonl. The walker that backs both the post-/rename
    // poll AND the pre-resume existence check must match it.
    use agman::harness::poll_session_index_for_test;

    let codex_home = tempfile::tempdir().unwrap();
    let index_path = codex_home.path().join("session_index.jsonl");
    let name = "agman-ceo";

    std::fs::write(
        &index_path,
        format!("{{\"thread_name\": \"{name}\", \"id\": \"abc-123\"}}\n"),
    )
    .unwrap();

    assert!(
        poll_session_index_for_test(&index_path, name, std::time::Duration::from_secs(2)),
        "walker must match `thread_name` (codex's actual key)"
    );

    // Forward-compat: still matches `name`.
    let other_idx = codex_home.path().join("session_index_v2.jsonl");
    std::fs::write(
        &other_idx,
        format!("{{\"name\": \"{name}\", \"id\": \"def-456\"}}\n"),
    )
    .unwrap();
    assert!(poll_session_index_for_test(
        &other_idx,
        name,
        std::time::Duration::from_secs(2)
    ));
}

#[test]
fn codex_register_session_name_polls_session_index() {
    // Simulate codex writing a session_index.jsonl entry with our name.
    // We bypass the actual /rename paste (no tmux available in tests) by
    // pre-seeding the index file before invoking the harness's session
    // index poller via the documented poll path.
    use agman::harness::poll_session_index_for_test;

    let codex_home = tempfile::tempdir().unwrap();
    let index_path = codex_home.path().join("session_index.jsonl");
    let name = "agman-ceo";

    // Write the entry. The harness should observe it via the polling helper.
    std::fs::write(
        &index_path,
        format!("{{\"name\": \"{name}\", \"id\": \"abc-123\"}}\n"),
    )
    .unwrap();

    assert!(poll_session_index_for_test(
        &index_path,
        name,
        std::time::Duration::from_secs(2)
    ));
    // Negative case: a different name is not found.
    assert!(!poll_session_index_for_test(
        &index_path,
        "agman-other",
        std::time::Duration::from_millis(300)
    ));
}
