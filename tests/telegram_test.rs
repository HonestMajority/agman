use agman::telegram::{classify_outbox_result, OutboxAction, TgError};

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
