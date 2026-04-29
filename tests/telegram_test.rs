mod helpers;

use agman::telegram::{
    classify_outbox_result, format_reply_message, format_sender_tag, parent_of, parse_sender_tag,
    resolve_tag_to_agent, run_iter_catching_panic, OutboxAction, TgError,
};

use helpers::{create_test_project, create_test_researcher, test_config};

#[test]
fn classify_ok_marks_delivered() {
    assert_eq!(classify_outbox_result(Ok(())), OutboxAction::MarkDelivered);
}

#[test]
fn classify_permanent_dead_letters() {
    assert_eq!(
        classify_outbox_result(Err(TgError::Permanent)),
        OutboxAction::DeadLetter
    );
}

#[test]
fn classify_transient_stops() {
    assert_eq!(
        classify_outbox_result(Err(TgError::Transient)),
        OutboxAction::Stop
    );
}

#[test]
fn panic_in_iteration_is_caught_and_classified() {
    // Suppress the default panic hook for this test so we don't pollute test
    // output with the expected backtrace from `panic!("boom")`.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let panic_result = run_iter_catching_panic(|| panic!("boom"));
    let ok_result = run_iter_catching_panic(|| ());
    std::panic::set_hook(prev);

    assert_eq!(panic_result, Err("boom".to_string()));
    assert!(ok_result.is_ok());
}

#[test]
fn format_sender_tag_cases() {
    assert_eq!(format_sender_tag("chief-of-staff"), "CoS");
    assert_eq!(format_sender_tag("pm-foo"), "PM:pm-foo");
    assert_eq!(format_sender_tag("researcher:proj--bar"), "R:bar");
    assert_eq!(format_sender_tag("researcher:chief-of-staff--baz"), "R:baz");
}

#[test]
fn parent_of_cases() {
    assert_eq!(parent_of("chief-of-staff"), None);
    assert_eq!(
        parent_of("some-project"),
        Some("chief-of-staff".to_string())
    );
    assert_eq!(parent_of("researcher:proj--bar"), Some("proj".to_string()));
    assert_eq!(
        parent_of("researcher:chief-of-staff--baz"),
        Some("chief-of-staff".to_string())
    );
}

#[test]
fn parse_sender_tag_cases() {
    assert_eq!(parse_sender_tag("[CoS] hi"), Some("CoS"));
    assert_eq!(parse_sender_tag("[PM:foo] bar"), Some("PM:foo"));
    assert_eq!(parse_sender_tag("[R:baz] x"), Some("R:baz"));
    assert_eq!(parse_sender_tag("plain"), None);
    assert_eq!(parse_sender_tag("[unclosed"), None);
    // Bracket not at start.
    assert_eq!(parse_sender_tag(" [CoS] leading space"), None);
    // Empty tag is technically extractable.
    assert_eq!(parse_sender_tag("[] body"), Some(""));
}

#[test]
fn format_reply_message_strips_tag_and_concats_body() {
    let out = format_reply_message("[CoS] hello world", "thanks");
    assert_eq!(out, "In reply to: \"hello world\"\n\nthanks");
}

#[test]
fn format_reply_message_keeps_plain_original() {
    let out = format_reply_message("no tag here", "ok");
    assert_eq!(out, "In reply to: \"no tag here\"\n\nok");
}

#[test]
fn format_reply_message_truncates_long_snippet() {
    let long = format!("[CoS] {}", "x".repeat(200));
    let out = format_reply_message(&long, "body");
    // 140 chars + ellipsis, surrounded by "In reply to: \"...\"\n\nbody".
    let expected_snippet: String = "x".repeat(140);
    assert_eq!(out, format!("In reply to: \"{expected_snippet}…\"\n\nbody"));
}

#[test]
fn resolve_tag_chief_of_staff() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    assert_eq!(
        resolve_tag_to_agent(&config, "CoS"),
        Some("chief-of-staff".to_string())
    );
}

#[test]
fn resolve_tag_pm_existing() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _ = create_test_project(&config, "myproj");
    assert_eq!(
        resolve_tag_to_agent(&config, "PM:myproj"),
        Some("myproj".to_string())
    );
}

#[test]
fn resolve_tag_pm_missing_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    assert_eq!(resolve_tag_to_agent(&config, "PM:ghost"), None);
}

#[test]
fn resolve_tag_researcher_unique_match() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _ = create_test_project(&config, "alpha");
    let _ = create_test_researcher(&config, "alpha", "scout");
    assert_eq!(
        resolve_tag_to_agent(&config, "R:scout"),
        Some("researcher:alpha--scout".to_string())
    );
}

#[test]
fn resolve_tag_researcher_ambiguous_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _ = create_test_project(&config, "alpha");
    let _ = create_test_project(&config, "beta");
    let _ = create_test_researcher(&config, "alpha", "twin");
    let _ = create_test_researcher(&config, "beta", "twin");
    assert_eq!(resolve_tag_to_agent(&config, "R:twin"), None);
}

#[test]
fn resolve_tag_researcher_missing_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    assert_eq!(resolve_tag_to_agent(&config, "R:nobody"), None);
}

#[test]
fn resolve_tag_unknown_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    assert_eq!(resolve_tag_to_agent(&config, "UNKNOWN"), None);
    assert_eq!(resolve_tag_to_agent(&config, ""), None);
    assert_eq!(resolve_tag_to_agent(&config, "PM:"), None);
    assert_eq!(resolve_tag_to_agent(&config, "R:"), None);
}
