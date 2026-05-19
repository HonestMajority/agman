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
