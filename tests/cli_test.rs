#[test]
fn cli_help_exposes_agent_commands() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agman"))
        .arg("--help")
        .output()
        .expect("failed to run agman --help");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("help output should be utf8");

    assert!(stdout.contains("create-agent"));
    assert!(stdout.contains("list-agents"));
    assert!(stdout.contains("archive-agent"));
    assert!(stdout.contains("attach-agent"));
    assert!(stdout.contains("move-agent"));
    assert!(stdout.contains("detach-agent"));
    assert!(stdout.contains("send-message"));
    assert!(stdout.contains("link-pr"));
}

#[test]
fn cli_attach_agent_help_exposes_pm_facing_syntax() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agman"))
        .args(["attach-agent", "--help"])
        .output()
        .expect("failed to run agman attach-agent --help");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("help output should be utf8");

    assert!(stdout.contains(
        "agman attach-agent --project backend --name api-investigator --task backend--fix-login"
    ));
    assert!(stdout.contains("--role-label"));
}

#[test]
fn cli_link_pr_help_exposes_task_pr_linking_syntax() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agman"))
        .args(["link-pr", "--help"])
        .output()
        .expect("failed to run agman link-pr --help");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("help output should be utf8");

    assert!(stdout.contains("agman link-pr backend--fix-login"));
    assert!(stdout.contains("--from-sidecar"));
    assert!(stdout.contains("--force"));
    assert!(stdout.contains("--not-owned"));
}

#[test]
fn cli_create_pm_task_help_exposes_first_prompt_not_description() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agman"))
        .args(["create-pm-task", "--help"])
        .output()
        .expect("failed to run agman create-pm-task --help");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("help output should be utf8");

    assert!(stdout.contains("--first-prompt <FIRST_PROMPT>"));
    assert!(stdout.contains("-d"));
    assert!(stdout.contains("Optional first prompt sent to the attached engineer"));
    assert!(stdout.contains("agman create-pm-task myproj myrepo fix-bug --first-prompt"));
    assert!(!stdout.contains("--description"));
}

#[test]
fn cli_create_agent_help_exposes_first_prompt_not_description() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agman"))
        .args(["create-agent", "--help"])
        .output()
        .expect("failed to run agman create-agent --help");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("help output should be utf8");

    assert!(stdout.contains("--first-prompt <FIRST_PROMPT>"));
    assert!(stdout.contains("-d"));
    assert!(stdout.contains("Optional first prompt sent to the agent inbox"));
    assert!(stdout.contains("agman create-agent --kind researcher"));
    assert!(stdout.contains("--first-prompt"));
    assert!(!stdout.contains("--description"));
}

#[test]
fn cli_role_agent_help_exposes_first_prompt_not_description() {
    for command in [
        "create-researcher",
        "create-operator",
        "create-reviewer",
        "create-tester",
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_agman"))
            .args([command, "--help"])
            .output()
            .unwrap_or_else(|_| panic!("failed to run agman {command} --help"));

        assert!(output.status.success(), "{command} --help failed");
        let stdout = String::from_utf8(output.stdout).expect("help output should be utf8");

        assert!(
            stdout.contains("--first-prompt <FIRST_PROMPT>"),
            "{command} help did not expose --first-prompt"
        );
        assert!(stdout.contains("-d"), "{command} help did not expose -d");
        assert!(
            !stdout.contains("--description"),
            "{command} help exposed hidden --description alias"
        );
    }
}
