mod helpers;

use agman::{harness, inbox, sender_auth};
use helpers::*;
use std::process::{Command, Output};

fn send(command: &mut Command, target: &str, from: &str) -> Output {
    command
        .args([
            "send-message",
            target,
            "--from",
            from,
            "PRIVATE_MESSAGE_BODY",
        ])
        .output()
        .unwrap()
}

fn assert_rejected(output: Output) {
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("sender"), "{error}");
    assert!(!error.contains("PRIVATE_MESSAGE_BODY"));
    assert!(!error.contains("WRONG_TOKEN"));
}

#[test]
fn exact_sender_ownership_is_required_before_any_append() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    create_test_project(&config, "project");
    create_test_researcher(&config, "project", "a");
    create_test_researcher(&config, "project", "b");
    std::fs::create_dir_all(config.chief_of_staff_dir()).unwrap();
    let a = "researcher:project--a";
    let b = "researcher:project--b";

    // Valid and replyable is insufficient, even after a token has been minted.
    for sender in ["project", a, b, "chief-of-staff"] {
        sender_auth::launch_command(&config, sender, "true").unwrap();
        assert_rejected(send(&mut isolated_cli(&config), "project", sender));
    }
    for from in [a, b] {
        assert_rejected(send(
            &mut authenticated_cli(&config, "project"),
            "project",
            from,
        ));
    }
    assert_rejected(send(&mut authenticated_cli(&config, a), "project", b));
    for sender in [
        "telegram",
        "system",
        "user",
        "codex",
        "unknown",
        "",
        " project",
        "project ",
        "./project",
        "researcher:project--missing",
        "engineer:project--a",
    ] {
        assert_rejected(send(
            &mut authenticated_cli(&config, "project"),
            "project",
            sender,
        ));
    }
    for (env_sender, env_token) in [
        (Some("project"), None),
        (None, Some("WRONG_TOKEN")),
        (Some("project"), Some("")),
        (Some("project"), Some("WRONG_TOKEN")),
        (Some("project "), Some("WRONG_TOKEN")),
    ] {
        let mut command = isolated_cli(&config);
        if let Some(sender) = env_sender {
            command.env("AGMAN_SENDER", sender);
        }
        if let Some(token) = env_token {
            command.env("AGMAN_SENDER_TOKEN", token);
        }
        assert_rejected(send(&mut command, "project", "project"));
    }
    let mut missing_token = authenticated_cli(&config, "project");
    std::fs::remove_file(config.project_dir("project").join("sender-token")).unwrap();
    assert_rejected(send(&mut missing_token, "project", "project"));
    let alias_token =
        std::fs::read_to_string(config.agent_dir("project", "a").join("sender-token")).unwrap();
    let mut alias = isolated_cli(&config);
    alias
        .env("AGMAN_SENDER", "engineer:project--a")
        .env("AGMAN_SENDER_TOKEN", alias_token);
    assert_rejected(send(&mut alias, "project", "engineer:project--a"));
    assert!(inbox::read_messages(&config.project_inbox("project"))
        .unwrap()
        .is_empty());
}

#[test]
fn authenticated_sends_record_process_provenance_without_copying_body_or_token() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    create_test_project(&config, "project");
    create_test_researcher(&config, "project", "a");
    std::fs::create_dir_all(config.chief_of_staff_dir()).unwrap();
    let a = "researcher:project--a";
    for (sender, target) in [
        ("project", a),
        ("project", "project"),
        (a, "project"),
        ("chief-of-staff", "project"),
        ("chief-of-staff", "telegram"),
    ] {
        let mut command = authenticated_cli(&config, sender);
        command.current_dir(tmp.path());
        let output = send(&mut command, target, sender);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let path = agman::use_cases::agent_inbox_path(&config, target).unwrap();
        let messages = inbox::read_messages(&path).unwrap();
        let message = messages.last().unwrap();
        assert_eq!(message.from, sender);
        let provenance = message.provenance.as_ref().unwrap();
        assert_eq!(provenance.authenticated_sender, sender);
        assert_eq!(provenance.target, target);
        assert_ne!(provenance.pid, 0);
        #[cfg(unix)]
        assert_eq!(provenance.ppid, Some(std::process::id()));
        assert_eq!(
            provenance.cwd.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
        assert_eq!(
            provenance
                .executable
                .as_ref()
                .unwrap()
                .canonicalize()
                .unwrap(),
            std::path::Path::new(env!("CARGO_BIN_EXE_agman"))
                .canonicalize()
                .unwrap()
        );
        assert_eq!(
            provenance.argv0.as_deref(),
            Some(env!("CARGO_BIN_EXE_agman"))
        );
        assert!(message.timestamp <= chrono::Utc::now());
        let json = serde_json::to_string(provenance).unwrap();
        assert!(json.contains("\"source\":\"cli\""));
        assert!(!json.contains("PRIVATE_MESSAGE_BODY"));
        let token_path = agman::use_cases::agent_inbox_path(&config, sender)
            .unwrap()
            .parent()
            .unwrap()
            .join("sender-token");
        let token = std::fs::read_to_string(token_path).unwrap();
        assert!(!json.contains(&token));
        let log = std::fs::read_to_string(config.base_dir.join("agman.log")).unwrap();
        assert!(!log.contains("PRIVATE_MESSAGE_BODY"));
        assert!(!log.contains(&token));
    }
}

#[test]
fn internal_pseudo_senders_append_without_cli_credentials() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("inbox.jsonl");
    for sender in ["telegram", "system", "project"] {
        let row = inbox::append_message(&path, sender, "internal event").unwrap();
        assert!(row.provenance.is_none());
    }
    agman::use_cases::request_handoff(&path, "system", tmp.path()).unwrap();
    let messages = inbox::read_messages(&path).unwrap();
    assert_eq!(messages.len(), 4);
    assert!(messages[3].message.starts_with("[HANDOFF REQUEST]"));
    assert!(messages[3].provenance.is_none());
}

#[test]
fn tokens_are_stable_private_and_not_embedded_in_launch_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    create_test_project(&config, "project");
    let barrier = std::sync::Barrier::new(8);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let barrier = &barrier;
            let config = &config;
            scope.spawn(move || {
                barrier.wait();
                sender_auth::launch_command(config, "project", "true").unwrap()
            });
        }
    });
    let command = sender_auth::launch_command(&config, "project", "true").unwrap();
    let path = config.project_dir("project").join("sender-token");
    let token = std::fs::read_to_string(&path).unwrap();
    assert_eq!(token.len(), 64);
    assert!(!command.contains(&token));
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| sender_auth::launch_command(&config, "project", "true").unwrap());
        }
    });
    assert_eq!(std::fs::read_to_string(&path).unwrap(), token);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let mut command = authenticated_cli(&config, "project");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(sender_auth::launch_command(&config, "project", "true").is_err());
        assert_rejected(send(&mut command, "project", "project"));
    }
}

#[cfg(unix)]
#[test]
fn token_symlinks_and_corrupt_tokens_fail_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    create_test_project(&config, "project");
    let mut command = authenticated_cli(&config, "project");
    let path = config.project_dir("project").join("sender-token");
    let moved = tmp.path().join("moved-token");
    std::fs::rename(&path, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &path).unwrap();
    assert!(sender_auth::launch_command(&config, "project", "true").is_err());
    assert_rejected(send(&mut command, "project", "project"));
    std::fs::remove_file(&path).unwrap();
    std::fs::rename(&moved, &path).unwrap();
    for token in [
        "",
        "INVALID",
        &"a".repeat(65),
        &format!("{}\n", "a".repeat(64)),
    ] {
        std::fs::write(&path, token).unwrap();
        assert!(sender_auth::launch_command(&config, "project", "true").is_err());
        assert_rejected(send(
            isolated_cli(&config)
                .env("AGMAN_SENDER", "project")
                .env("AGMAN_SENDER_TOKEN", token),
            "project",
            "project",
        ));
    }
}

#[cfg(unix)]
#[test]
fn fresh_and_resumed_harness_commands_pass_credentials_to_subprocesses() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::Builder::new()
        .prefix("agman's launch ")
        .tempdir()
        .unwrap();
    let config = test_config(&tmp);
    create_test_project(&config, "project");
    create_test_researcher(&config, "project", "a");
    std::fs::create_dir_all(config.chief_of_staff_dir()).unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    for kind in harness::HarnessKind::ALL {
        let stub = bin.join(kind.as_str());
        std::fs::write(&stub, "#!/bin/sh\nexec \"$AGMAN_TEST_BINARY\" send-message project --from \"$AGMAN_SENDER\" launched\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
        for sender in ["chief-of-staff", "project", "researcher:project--a"] {
            for session_key in [
                harness::SessionKey::Auto,
                harness::SessionKey::Pin("session-id"),
                harness::SessionKey::Resume("session-id"),
            ] {
                let command = kind
                    .select()
                    .build_session_command(&harness::LaunchContext {
                        identity: "test identity",
                        name: "test-session",
                        identity_file: Some(tmp.path()),
                        session_dir: Some(tmp.path()),
                        cwd: tmp.path(),
                        no_alt_screen: true,
                        capabilities: Default::default(),
                        session_key,
                    });
                let command = sender_auth::launch_command(&config, sender, &command).unwrap();
                let output = Command::new("sh")
                    .args(["-c", &command])
                    .env("HOME", tmp.path())
                    .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
                    .env("AGMAN_TEST_BINARY", env!("CARGO_BIN_EXE_agman"))
                    .env("AGMAN_SENDER", "wrong-inherited-sender")
                    .env("AGMAN_SENDER_TOKEN", "wrong-inherited-token")
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }
    assert_eq!(
        inbox::read_messages(&config.project_inbox("project"))
            .unwrap()
            .len(),
        27
    );
}
