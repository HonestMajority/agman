use agman::telegram::{classify_outbox_result, run_iter_catching_panic, OutboxAction, TgError};

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
