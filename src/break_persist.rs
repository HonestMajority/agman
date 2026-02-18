use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Load a persisted break reset timestamp from disk and convert it to an `Instant`.
///
/// Returns `None` if the file doesn't exist, can't be parsed, or the timestamp
/// is older than `max_age` (stale — e.g. agman was closed overnight).
pub fn load_break_reset(path: &Path, max_age: Duration) -> Option<Instant> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return None,
    };

    let epoch_secs: u64 = match contents.trim().parse() {
        Ok(s) => s,
        Err(_) => {
            tracing::warn!(path = %path.display(), "failed to parse break state file, ignoring");
            return None;
        }
    };

    let saved_time = UNIX_EPOCH + Duration::from_secs(epoch_secs);
    let now_sys = SystemTime::now();

    let age = match now_sys.duration_since(saved_time) {
        Ok(d) => d,
        Err(_) => {
            // saved_time is in the future (clock was adjusted) — treat as fresh
            tracing::info!(path = %path.display(), "break state timestamp is in the future, treating as now");
            return Some(Instant::now());
        }
    };

    if age > max_age {
        tracing::info!(
            age_secs = age.as_secs(),
            max_age_secs = max_age.as_secs(),
            "persisted break timer is stale, starting fresh"
        );
        return None;
    }

    // Convert: the saved time was `age` seconds ago, so the equivalent Instant
    // is `Instant::now() - age`. Use checked_sub to handle edge cases.
    let instant = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);

    tracing::info!(
        age_secs = age.as_secs(),
        "restored persisted break timer"
    );

    Some(instant)
}

/// Save the current break reset time to disk as a Unix epoch timestamp (seconds).
pub fn save_break_reset(path: &Path, last_break_reset: &Instant) {
    let elapsed = last_break_reset.elapsed();
    let now_sys = SystemTime::now();

    // The break was reset `elapsed` seconds ago
    let reset_time = match now_sys.checked_sub(elapsed) {
        Some(t) => t,
        None => now_sys, // shouldn't happen, but fall back to now
    };

    let epoch_secs = match reset_time.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs(),
        Err(_) => return, // before epoch — shouldn't happen
    };

    if let Err(e) = std::fs::write(path, epoch_secs.to_string()) {
        tracing::warn!(path = %path.display(), error = %e, "failed to save break state");
    }
}
