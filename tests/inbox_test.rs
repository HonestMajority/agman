use agman::config::Config;
use agman::inbox::{self, InboxMessage};
use agman::project::Project;
use chrono::Utc;
use std::collections::HashSet;
use std::process::{Command, Stdio};

fn message_line(seq: u64, message: &str) -> String {
    serde_json::to_string(&InboxMessage {
        seq,
        from: "tester".to_string(),
        message: message.to_string(),
        timestamp: Utc::now(),
    })
    .unwrap()
}

#[test]
fn append_message_refuses_parse_poisoned_inbox_and_preserves_file() {
    let tmp = tempfile::tempdir().unwrap();
    let inbox_path = tmp.path().join("project").join("inbox.jsonl");
    std::fs::create_dir_all(inbox_path.parent().unwrap()).unwrap();

    let original = format!(
        "{}\n{}{}\n",
        message_line(1, "valid"),
        message_line(83, "poison-a"),
        message_line(83, "poison-b")
    );
    std::fs::write(&inbox_path, &original).unwrap();
    let before = std::fs::read(&inbox_path).unwrap();

    let err = inbox::append_message(&inbox_path, "tester", "should not append")
        .unwrap_err()
        .to_string();

    assert!(err.contains(&inbox_path.display().to_string()));
    assert!(err.contains("line 2"));
    assert_eq!(std::fs::read(&inbox_path).unwrap(), before);
}

#[test]
fn read_messages_reports_corrupt_line_number_without_body() {
    let tmp = tempfile::tempdir().unwrap();
    let inbox_path = tmp.path().join("inbox.jsonl");
    let raw_secret = "RAW_MESSAGE_BODY_SHOULD_NOT_APPEAR";
    let contents = format!(
        "{}\n{}{}\n",
        message_line(1, "valid"),
        message_line(2, raw_secret),
        message_line(3, "second object on same line")
    );
    std::fs::write(&inbox_path, contents).unwrap();

    let err = inbox::read_messages(&inbox_path).unwrap_err();
    let error_text = format!("{err:#}");

    assert!(error_text.contains(&inbox_path.display().to_string()));
    assert!(error_text.contains("line 2"));
    assert!(!error_text.contains(raw_secret));
    assert!(!error_text.contains("second object on same line"));
}

#[test]
fn concurrent_send_message_processes_assign_unique_monotonic_seq_without_framing_corruption() {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config::new(tmp.path().join(".agman"), tmp.path().join("repos"));
    Project::create(&config, "race", "Race project").unwrap();

    let process_count = 32usize;
    let mut children = Vec::new();
    for i in 0..process_count {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agman"));
        command
            .env("HOME", tmp.path())
            .env("RUST_LOG", "off")
            .args([
                "send-message",
                "race",
                &format!("message-{i}"),
                "--from",
                &format!("sender-{i}"),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        children.push((i, command.spawn().unwrap()));
    }

    for (i, child) in children {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "send-message process {i} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let inbox_path = config.project_inbox("race");
    let messages = inbox::read_messages(&inbox_path).unwrap();
    assert_eq!(messages.len(), process_count);

    let seqs: Vec<u64> = messages.iter().map(|message| message.seq).collect();
    assert_eq!(
        seqs,
        (1..=process_count as u64).collect::<Vec<_>>(),
        "seqs should be monotonic in file order"
    );
    assert_eq!(
        seqs.iter().copied().collect::<HashSet<_>>().len(),
        process_count
    );

    let contents = std::fs::read_to_string(&inbox_path).unwrap();
    assert!(!contents.contains("}{"));
    assert_eq!(
        contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        process_count
    );
}
