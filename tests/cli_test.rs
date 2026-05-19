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
    assert!(stdout.contains("send-message"));
}
