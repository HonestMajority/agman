use agman::harness::{HarnessKind, LaunchContext, SessionKey};

fn cwd() -> std::path::PathBuf {
    std::env::temp_dir()
}

#[test]
fn harness_kind_round_trips_through_strings() {
    assert_eq!("claude".parse::<HarnessKind>(), Ok(HarnessKind::Claude));
    assert_eq!("codex".parse::<HarnessKind>(), Ok(HarnessKind::Codex));
    assert_eq!("goose".parse::<HarnessKind>(), Ok(HarnessKind::Goose));
    assert_eq!("nope".parse::<HarnessKind>(), Err(()));
    assert_eq!(HarnessKind::Claude.as_str(), "claude");
    assert_eq!(HarnessKind::Codex.as_str(), "codex");
    assert_eq!(HarnessKind::Goose.as_str(), "goose");
    assert_eq!(
        HarnessKind::ALL,
        &[HarnessKind::Claude, HarnessKind::Codex, HarnessKind::Goose]
    );
}

#[test]
fn claude_build_session_command_emits_system_prompt_and_name() {
    let h = HarnessKind::Claude.select();
    let cmd = h.build_session_command(&LaunchContext {
        identity: "Identity body",
        name: "agman-task-myrepo--feat-x-step-1",
        identity_file: None,
        cwd: &cwd(),
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
        identity_file: None,
        cwd: &cwd(),
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
        name: "agman-chief-of-staff",
        identity_file: None,
        cwd: &cwd(),
        no_alt_screen: false,
        session_key: SessionKey::Pin(uuid),
    });
    assert!(cmd.contains(&format!("--session-id '{uuid}'")));
    assert!(cmd.contains("--system-prompt 'Identity body'"));
    assert!(cmd.contains("--name 'agman-chief-of-staff'"));
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
        name: "agman-chief-of-staff",
        identity_file: None,
        cwd: &cwd(),
        no_alt_screen: false,
        session_key: SessionKey::Resume(uuid),
    });
    assert!(cmd.contains(&format!("--resume '{uuid}'")));
    // Resume must NOT pass --name: claude reattaches by stored name and
    // re-passing it on every resume risks overwriting the display name (and
    // would silently drift if the deterministic name ever changed between
    // launches).
    assert!(
        !cmd.contains("--name"),
        "resume must NOT pass --name: {cmd}"
    );
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
        identity_file: None,
        cwd: &cwd(),
        no_alt_screen: true,
        session_key: SessionKey::Auto,
    });
    assert!(cmd.starts_with("codex"));
    assert!(
        !cmd.contains("--skip-git-repo-check"),
        "codex 0.125.0 dropped this flag: {cmd}"
    );
    assert!(cmd.contains("--dangerously-bypass-approvals-and-sandbox"));
    assert!(cmd.contains("--no-alt-screen"));
    assert!(cmd.contains("developer_instructions=\"\"\"Identity body\"\"\""));
    // Codex doesn't take --name; the name is registered post-launch via /rename.
    assert!(!cmd.contains("--name"));
    assert!(!cmd.contains("--resume"));
    assert!(!cmd.contains(" resume "));
}

#[test]
fn codex_build_session_command_does_not_emit_skip_git_repo_check() {
    // Regression: codex 0.125.0 dropped `--skip-git-repo-check`. Emitting
    // it causes `error: unexpected argument '--skip-git-repo-check'` at
    // launch, dropping the tmux pane to a shell prompt and breaking the
    // downstream `/rename` paste-inject (which then runs as a shell command:
    // `zsh: no such file or directory: /rename`). Verify the flag is absent
    // for every session-key shape the codex builder emits.
    let h = HarnessKind::Codex.select();
    let work_dir = cwd();
    for key in [
        SessionKey::Auto,
        SessionKey::Pin("agman-pin"),
        SessionKey::Resume("agman-resume"),
    ] {
        let cmd = h.build_session_command(&LaunchContext {
            identity: "Identity body",
            name: "agman-task-myrepo--feat-x-step-1",
            identity_file: None,
            cwd: &work_dir,
            no_alt_screen: true,
            session_key: key,
        });
        assert!(
            !cmd.contains("--skip-git-repo-check"),
            "codex 0.125.0 dropped this flag; got: {cmd}"
        );
    }
}

#[test]
fn codex_build_session_command_always_bypasses_approvals_and_sandbox() {
    // Regression: codex must ALWAYS launch with
    // `--dangerously-bypass-approvals-and-sandbox` (mirrors claude's
    // `--dangerously-skip-permissions`). Without it, codex prompts before
    // privileged-feeling shell commands (e.g. `git add`), deadlocking
    // autonomous agman flows. Verify the flag is present for every
    // session-key shape the codex builder emits.
    let h = HarnessKind::Codex.select();
    let work_dir = cwd();
    for key in [
        SessionKey::Auto,
        SessionKey::Pin("agman-pin"),
        SessionKey::Resume("agman-resume"),
    ] {
        let cmd = h.build_session_command(&LaunchContext {
            identity: "Identity body",
            name: "agman-task-myrepo--feat-x-step-1",
            identity_file: None,
            cwd: &work_dir,
            no_alt_screen: true,
            session_key: key,
        });
        assert!(
            cmd.contains("--dangerously-bypass-approvals-and-sandbox"),
            "codex must always bypass approvals+sandbox; got: {cmd}"
        );
    }
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
        name: "agman-chief-of-staff",
        identity_file: None,
        cwd: &work_dir,
        no_alt_screen: true,
        session_key: SessionKey::Resume("agman-chief-of-staff"),
    });
    assert!(cmd.starts_with("codex"));
    assert!(cmd.contains(" resume 'agman-chief-of-staff'"));
    assert!(cmd.contains(&format!(" -C '{}'", work_dir.to_string_lossy())));
    assert!(cmd.contains("--dangerously-bypass-approvals-and-sandbox"));
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
        identity_file: None,
        cwd: &cwd(),
        no_alt_screen: false,
        session_key: SessionKey::Auto,
    });
    assert!(cmd.contains("Pre \\\"\\\"\\\" mid"));
    assert!(!cmd.contains("Pre \"\"\" mid"));
}

#[test]
fn goose_build_session_command_emits_auto_mode_moim_and_name() {
    let h = HarnessKind::Goose.select();
    let tmp = tempfile::tempdir().unwrap();
    let identity_file = tmp.path().join("identity").join("agman goose's name.md");
    let cmd = h.build_session_command(&LaunchContext {
        identity: "Identity body",
        name: "agman-goose's-name",
        identity_file: Some(&identity_file),
        cwd: &cwd(),
        no_alt_screen: false,
        session_key: SessionKey::Auto,
    });
    assert!(cmd.starts_with("GOOSE_MODE=auto "));
    assert!(cmd.contains("GOOSE_MOIM_MESSAGE_FILE="));
    assert!(cmd.contains("goose session"));
    assert!(cmd.contains("--with-builtin developer,tom"));
    assert!(cmd.contains("--name 'agman-goose'\\''s-name'"));
    assert!(cmd.contains("agman goose'\\''s name.md"));
    assert!(!cmd.contains("--resume"));
}

#[test]
fn goose_build_session_command_resumes_by_name() {
    let h = HarnessKind::Goose.select();
    let tmp = tempfile::tempdir().unwrap();
    let identity_file = tmp.path().join("identity").join("agman-goose.md");
    let cmd = h.build_session_command(&LaunchContext {
        identity: "Identity body",
        name: "agman-goose",
        identity_file: Some(&identity_file),
        cwd: &cwd(),
        no_alt_screen: false,
        session_key: SessionKey::Resume("agman-goose"),
    });
    assert!(cmd.contains("goose session"));
    assert!(cmd.contains("--resume --name 'agman-goose'"));
    assert!(!cmd.contains("Identity body"));
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
    assert_eq!(HarnessKind::Goose.select().skill_hint(), "");
}

#[test]
fn install_hints_match_documented_text() {
    assert!(HarnessKind::Claude
        .select()
        .install_hint()
        .contains("@anthropic-ai/claude-code"));
    let codex_hint = HarnessKind::Codex.select().install_hint();
    assert!(codex_hint.contains("codex") && codex_hint.contains("brew"));
    assert!(HarnessKind::Goose.select().install_hint().contains("Goose"));
}

#[test]
fn cli_binaries_match_kinds() {
    assert_eq!(HarnessKind::Claude.select().cli_binary(), "claude");
    assert_eq!(HarnessKind::Codex.select().cli_binary(), "codex");
    assert_eq!(HarnessKind::Goose.select().cli_binary(), "goose");
}

#[test]
fn codex_session_index_walker_matches_thread_name() {
    // Regression: codex writes `thread_name` (not `name`) in
    // session_index.jsonl. The walker that backs both the post-/rename
    // poll AND the pre-resume existence check must match it.
    use agman::harness::poll_session_index_for_test;

    let codex_home = tempfile::tempdir().unwrap();
    let index_path = codex_home.path().join("session_index.jsonl");
    let name = "agman-chief-of-staff";

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
    let name = "agman-chief-of-staff";

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

#[test]
fn codex_register_session_name_retries_until_indexed() {
    // Pin: codex /rename can land in a half-mounted bracket-paste UI on
    // step 2+ relaunches. The retry helper retries until the entry shows
    // up in session_index.jsonl. Simulate codex writing the entry only on
    // the second paste attempt; assert the helper returns Ok(true).
    use agman::harness::register_session_name_with_retry_for_test;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    let codex_home = tempfile::tempdir().unwrap();
    let index_path = codex_home.path().join("session_index.jsonl");
    let name = "agman-task-foo--bar-step-2";

    let attempts = Arc::new(AtomicU32::new(0));
    let attempts_in_paste = Arc::clone(&attempts);
    let index_path_in_paste = index_path.clone();
    let name_owned = name.to_string();

    let paste = Box::new(move || {
        let n = attempts_in_paste.fetch_add(1, Ordering::SeqCst) + 1;
        if n == 2 {
            // Simulate codex's TUI finally accepting the paste and writing
            // the index entry. Spawn a side thread so the entry shows up
            // *during* the poll window of attempt 2, not before paste returns.
            let p = index_path_in_paste.clone();
            let nm = name_owned.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                std::fs::write(&p, format!("{{\"thread_name\":\"{nm}\"}}\n")).unwrap();
            });
        }
        Ok(())
    });

    let result = register_session_name_with_retry_for_test(
        paste,
        &index_path,
        name,
        Duration::from_millis(0),
        Duration::from_millis(500),
        3,
    )
    .unwrap();
    assert!(result, "retry helper must return true once indexed");
    // Should have stopped at attempt 2 (the one that succeeded).
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[test]
fn codex_register_session_name_returns_ok_on_timeout_after_retries() {
    // Pin: when codex never indexes the rename (e.g., update prompt
    // swallows /rename), the retry helper must still return Ok(false)
    // after exhausting attempts. The session is still usable; just not
    // resume-by-name.
    use agman::harness::register_session_name_with_retry_for_test;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    let codex_home = tempfile::tempdir().unwrap();
    let index_path = codex_home.path().join("session_index.jsonl");
    let name = "agman-task-foo--bar-step-2";

    let attempts = Arc::new(AtomicU32::new(0));
    let attempts_in_paste = Arc::clone(&attempts);

    let paste = Box::new(move || {
        attempts_in_paste.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });

    let result = register_session_name_with_retry_for_test(
        paste,
        &index_path,
        name,
        Duration::from_millis(0),
        Duration::from_millis(150),
        3,
    )
    .unwrap();
    assert!(
        !result,
        "retry helper must return false after all attempts time out"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        3,
        "all 3 attempts must run when none succeed"
    );
}

// ---------------------------------------------------------------------------
// `ensure_workspace_trusted` — claude (`~/.claude.json`) and codex
// (`~/.codex/config.toml`).
//
// Tests use the explicit-path `ensure_workspace_trusted_for_test` seam so
// no process-global env var is mutated, keeping them parallel-safe.
// ---------------------------------------------------------------------------

#[test]
fn claude_ensure_workspace_trusted_creates_entry() {
    use agman::harness::{ensure_workspace_trusted_for_test, HarnessKind};

    let claude_home = tempfile::tempdir().unwrap();
    let trust_file = claude_home.path().join(".claude.json");
    // File doesn't exist yet — helper must create it.
    let cwd = tempfile::tempdir().unwrap();

    ensure_workspace_trusted_for_test(HarnessKind::Claude, &trust_file, cwd.path()).unwrap();

    let text = std::fs::read_to_string(&trust_file).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let cwd_str = cwd.path().to_string_lossy().to_string();
    assert_eq!(
        v.get("projects")
            .and_then(|p| p.get(&cwd_str))
            .and_then(|p| p.get("hasTrustDialogAccepted")),
        Some(&serde_json::Value::Bool(true))
    );
}

#[test]
fn claude_ensure_workspace_trusted_preserves_other_keys() {
    use agman::harness::{ensure_workspace_trusted_for_test, HarnessKind};

    let claude_home = tempfile::tempdir().unwrap();
    let trust_file = claude_home.path().join(".claude.json");

    // Pre-populate with several unrelated keys.
    let other_cwd = "/some/other/project";
    let pre = serde_json::json!({
        "anonymousId": "abc-123",
        "editorMode": "vim",
        "projects": {
            other_cwd: {
                "hasTrustDialogAccepted": true,
                "allowedTools": ["Bash", "Read"],
                "mcpServers": { "foo": { "command": "foo" } },
                "lastCost": 1.23,
            }
        }
    });
    std::fs::write(&trust_file, serde_json::to_string_pretty(&pre).unwrap()).unwrap();

    let cwd = tempfile::tempdir().unwrap();
    ensure_workspace_trusted_for_test(HarnessKind::Claude, &trust_file, cwd.path()).unwrap();

    let text = std::fs::read_to_string(&trust_file).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();

    // Root-level keys preserved.
    assert_eq!(
        v.get("anonymousId").and_then(|x| x.as_str()),
        Some("abc-123")
    );
    assert_eq!(v.get("editorMode").and_then(|x| x.as_str()), Some("vim"));

    // Other project entry preserved verbatim.
    let other = v
        .get("projects")
        .and_then(|p| p.get(other_cwd))
        .expect("other project preserved");
    assert_eq!(
        other.get("hasTrustDialogAccepted"),
        Some(&serde_json::Value::Bool(true))
    );
    assert_eq!(
        other.get("allowedTools"),
        Some(&serde_json::json!(["Bash", "Read"]))
    );
    assert_eq!(
        other.get("mcpServers"),
        Some(&serde_json::json!({ "foo": { "command": "foo" } }))
    );
    assert_eq!(other.get("lastCost").and_then(|x| x.as_f64()), Some(1.23));

    // Our cwd added with hasTrustDialogAccepted=true.
    let cwd_str = cwd.path().to_string_lossy().to_string();
    let ours = v
        .get("projects")
        .and_then(|p| p.get(&cwd_str))
        .expect("our cwd added");
    assert_eq!(
        ours.get("hasTrustDialogAccepted"),
        Some(&serde_json::Value::Bool(true))
    );
}

#[test]
fn claude_ensure_workspace_trusted_idempotent() {
    use agman::harness::{ensure_workspace_trusted_for_test, HarnessKind};

    let claude_home = tempfile::tempdir().unwrap();
    let trust_file = claude_home.path().join(".claude.json");
    let cwd = tempfile::tempdir().unwrap();

    ensure_workspace_trusted_for_test(HarnessKind::Claude, &trust_file, cwd.path()).unwrap();
    let bytes_before = std::fs::read(&trust_file).unwrap();
    let mtime_before = std::fs::metadata(&trust_file)
        .and_then(|m| m.modified())
        .unwrap();

    // Sleep just enough that any rewrite would bump the mtime.
    std::thread::sleep(std::time::Duration::from_millis(20));
    ensure_workspace_trusted_for_test(HarnessKind::Claude, &trust_file, cwd.path()).unwrap();
    let bytes_after = std::fs::read(&trust_file).unwrap();
    let mtime_after = std::fs::metadata(&trust_file)
        .and_then(|m| m.modified())
        .unwrap();

    assert_eq!(
        bytes_before, bytes_after,
        "second call must not rewrite the file"
    );
    assert_eq!(mtime_before, mtime_after, "second call must not bump mtime");
}

#[test]
fn claude_ensure_workspace_trusted_upgrades_false_to_true() {
    use agman::harness::{ensure_workspace_trusted_for_test, HarnessKind};

    let claude_home = tempfile::tempdir().unwrap();
    let trust_file = claude_home.path().join(".claude.json");
    let cwd = tempfile::tempdir().unwrap();
    let cwd_str = cwd.path().to_string_lossy().to_string();

    let pre = serde_json::json!({
        "projects": {
            &cwd_str: {
                "hasTrustDialogAccepted": false,
                "allowedTools": ["Bash"]
            }
        }
    });
    std::fs::write(&trust_file, serde_json::to_string_pretty(&pre).unwrap()).unwrap();

    ensure_workspace_trusted_for_test(HarnessKind::Claude, &trust_file, cwd.path()).unwrap();

    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&trust_file).unwrap()).unwrap();
    let project = v
        .get("projects")
        .and_then(|p| p.get(&cwd_str))
        .expect("project entry preserved");
    assert_eq!(
        project.get("hasTrustDialogAccepted"),
        Some(&serde_json::Value::Bool(true))
    );
    // Other sub-keys preserved untouched.
    assert_eq!(
        project.get("allowedTools"),
        Some(&serde_json::json!(["Bash"]))
    );
}

#[test]
fn codex_ensure_workspace_trusted_creates_entry() {
    use agman::harness::{ensure_workspace_trusted_for_test, HarnessKind};

    let codex_home = tempfile::tempdir().unwrap();
    let trust_file = codex_home.path().join("config.toml");
    let cwd = tempfile::tempdir().unwrap();

    ensure_workspace_trusted_for_test(HarnessKind::Codex, &trust_file, cwd.path()).unwrap();

    let text = std::fs::read_to_string(&trust_file).unwrap();
    let v: toml::Value = toml::from_str(&text).unwrap();
    let cwd_str = cwd.path().to_string_lossy().to_string();
    let trust_level = v
        .get("projects")
        .and_then(|p| p.as_table())
        .and_then(|p| p.get(&cwd_str))
        .and_then(|p| p.get("trust_level"))
        .and_then(|s| s.as_str());
    assert_eq!(trust_level, Some("trusted"));
}

#[test]
fn codex_ensure_workspace_trusted_preserves_other_keys() {
    use agman::harness::{ensure_workspace_trusted_for_test, HarnessKind};

    let codex_home = tempfile::tempdir().unwrap();
    let trust_file = codex_home.path().join("config.toml");

    let pre = r#"
model = "gpt-5"
sandbox_mode = "workspace-write"

[projects."/some/other/project"]
trust_level = "trusted"

[projects."/yet/another"]
trust_level = "untrusted"
"#;
    std::fs::write(&trust_file, pre).unwrap();

    let cwd = tempfile::tempdir().unwrap();
    ensure_workspace_trusted_for_test(HarnessKind::Codex, &trust_file, cwd.path()).unwrap();

    let v: toml::Value = toml::from_str(&std::fs::read_to_string(&trust_file).unwrap()).unwrap();

    // Root-level keys preserved.
    assert_eq!(v.get("model").and_then(|x| x.as_str()), Some("gpt-5"));
    assert_eq!(
        v.get("sandbox_mode").and_then(|x| x.as_str()),
        Some("workspace-write")
    );

    // Other project tables preserved verbatim.
    let projects = v.get("projects").and_then(|p| p.as_table()).unwrap();
    assert_eq!(
        projects
            .get("/some/other/project")
            .and_then(|p| p.get("trust_level"))
            .and_then(|s| s.as_str()),
        Some("trusted")
    );
    assert_eq!(
        projects
            .get("/yet/another")
            .and_then(|p| p.get("trust_level"))
            .and_then(|s| s.as_str()),
        Some("untrusted"),
        "untrusted entry for an unrelated cwd must not be touched"
    );

    // Our cwd added with trust_level=trusted.
    let cwd_str = cwd.path().to_string_lossy().to_string();
    assert_eq!(
        projects
            .get(&cwd_str)
            .and_then(|p| p.get("trust_level"))
            .and_then(|s| s.as_str()),
        Some("trusted")
    );
}

#[test]
fn codex_ensure_workspace_trusted_idempotent() {
    use agman::harness::{ensure_workspace_trusted_for_test, HarnessKind};

    let codex_home = tempfile::tempdir().unwrap();
    let trust_file = codex_home.path().join("config.toml");
    let cwd = tempfile::tempdir().unwrap();

    ensure_workspace_trusted_for_test(HarnessKind::Codex, &trust_file, cwd.path()).unwrap();
    let bytes_before = std::fs::read(&trust_file).unwrap();
    let mtime_before = std::fs::metadata(&trust_file)
        .and_then(|m| m.modified())
        .unwrap();

    std::thread::sleep(std::time::Duration::from_millis(20));
    ensure_workspace_trusted_for_test(HarnessKind::Codex, &trust_file, cwd.path()).unwrap();
    let bytes_after = std::fs::read(&trust_file).unwrap();
    let mtime_after = std::fs::metadata(&trust_file)
        .and_then(|m| m.modified())
        .unwrap();

    assert_eq!(
        bytes_before, bytes_after,
        "second call must not rewrite the file"
    );
    assert_eq!(mtime_before, mtime_after, "second call must not bump mtime");
}

#[test]
fn codex_ensure_workspace_trusted_upgrades_untrusted_to_trusted() {
    use agman::harness::{ensure_workspace_trusted_for_test, HarnessKind};

    let codex_home = tempfile::tempdir().unwrap();
    let trust_file = codex_home.path().join("config.toml");
    let cwd = tempfile::tempdir().unwrap();
    let cwd_str = cwd.path().to_string_lossy().to_string();

    let pre = format!("[projects.\"{}\"]\ntrust_level = \"untrusted\"\n", cwd_str);
    std::fs::write(&trust_file, pre).unwrap();

    ensure_workspace_trusted_for_test(HarnessKind::Codex, &trust_file, cwd.path()).unwrap();

    let v: toml::Value = toml::from_str(&std::fs::read_to_string(&trust_file).unwrap()).unwrap();
    let trust_level = v
        .get("projects")
        .and_then(|p| p.get(&cwd_str))
        .and_then(|p| p.get("trust_level"))
        .and_then(|s| s.as_str());
    assert_eq!(
        trust_level,
        Some("trusted"),
        "untrusted should be upgraded to trusted on launch"
    );
}
