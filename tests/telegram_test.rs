use agman::telegram::{
    classify_outbox_result, format_sender_tag, parent_of, run_iter_catching_panic, OutboxAction,
    TgError,
};

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
    assert_eq!(format_sender_tag("ceo"), "CEO");
    assert_eq!(format_sender_tag("pm-foo"), "PM:pm-foo");
    assert_eq!(format_sender_tag("researcher:proj--bar"), "R:bar");
    assert_eq!(format_sender_tag("researcher:ceo--baz"), "R:baz");
}

#[test]
fn parent_of_cases() {
    assert_eq!(parent_of("ceo"), None);
    assert_eq!(parent_of("some-project"), Some("ceo".to_string()));
    assert_eq!(parent_of("researcher:proj--bar"), Some("proj".to_string()));
    assert_eq!(parent_of("researcher:ceo--baz"), Some("ceo".to_string()));
}
