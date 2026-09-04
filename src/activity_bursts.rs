use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

/// Samples arrive about once a second while the TUI is running; a larger gap
/// between two unconfirmed epochs means the earlier burst has ended. A
/// session unsampled for longer than this (a blocking attach, a restart) may
/// have completed a whole turn unobserved, so its next sample is trusted like
/// a first one unless it is the very epoch already judged a lone bump before
/// the gap.
pub const ACTIVITY_BURST_GAP_SECS: i64 = 10;
pub const ACTIVITY_BURST_GAP: Duration = Duration::from_secs(ACTIVITY_BURST_GAP_SECS as u64);

/// Output-burst confirmation for one tmux session. A newer `window_activity`
/// epoch counts as real agent activity only once output has spanned two
/// distinct seconds: an idle harness writes invisible single-second
/// housekeeping updates (Claude Code does so on timers and system wakes),
/// whereas a real turn repaints across many seconds. Until then samples are
/// rewound to `confirmed`.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct ActivityBurst {
    pub confirmed: i64,
    /// `(first, last)` epochs of an unconfirmed burst newer than `confirmed`.
    pub pending: Option<(i64, i64)>,
    /// Last sample by this process; `None` after a restart, so the first
    /// sample is judged like one following a sampling gap.
    #[serde(skip)]
    pub sampled_at: Option<Instant>,
}

impl ActivityBurst {
    /// Judge `activity` and return the epoch the sample must be rewound to
    /// when the burst is still unconfirmed.
    fn observe(&mut self, activity: i64, unobserved: bool) -> Option<i64> {
        if activity <= self.confirmed {
            self.pending = None;
            return None;
        }
        // An epoch already judged a lone bump stays one across a sampling
        // gap; only output the gap itself hid is trusted like a first sample.
        let known_lone_bump = self.pending.is_some_and(|(_, last)| activity == last);
        if unobserved && !known_lone_bump {
            self.confirmed = activity;
            self.pending = None;
            return None;
        }
        let (first, last) = match self.pending {
            Some((first, last)) if activity - last <= ACTIVITY_BURST_GAP_SECS => (first, activity),
            _ => (activity, activity),
        };
        if last > first {
            self.confirmed = activity;
            self.pending = None;
            None
        } else {
            self.pending = Some((first, last));
            Some(self.confirmed)
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Observation {
    pub rewind_to: Option<i64>,
    /// Persisted fields changed (a fresh `sampled_at` alone does not count).
    pub changed: bool,
}

/// Per-session burst state, persisted so a TUI restart keeps filtering ticks
/// it had already judged lone bumps.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ActivityBursts {
    pub sessions: HashMap<String, ActivityBurst>,
}

impl ActivityBursts {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|data| serde_json::from_str::<Self>(&data).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) {
        if let Ok(data) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, data);
        }
    }

    pub fn observe(&mut self, session: &str, activity: i64, now: Instant) -> Observation {
        let Some(burst) = self.sessions.get_mut(session) else {
            self.sessions.insert(
                session.to_string(),
                ActivityBurst {
                    confirmed: activity,
                    pending: None,
                    sampled_at: Some(now),
                },
            );
            return Observation {
                rewind_to: None,
                changed: true,
            };
        };
        let before = (burst.confirmed, burst.pending);
        let unobserved = burst
            .sampled_at
            .is_none_or(|at| now.duration_since(at) > ACTIVITY_BURST_GAP);
        burst.sampled_at = Some(now);
        let rewind_to = burst.observe(activity, unobserved);
        Observation {
            rewind_to,
            changed: (burst.confirmed, burst.pending) != before,
        }
    }

    /// Drop state for sessions no longer in the roster. Returns the number of
    /// entries pruned.
    pub fn retain_sessions(&mut self, roster: &HashSet<String>) -> usize {
        let before = self.sessions.len();
        self.sessions.retain(|session, _| roster.contains(session));
        let pruned = before - self.sessions.len();
        if pruned > 0 {
            tracing::debug!(pruned, "pruned activity-burst entries for absent sessions");
        }
        pruned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_corrupt_state_loads_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("activity_bursts.json");
        assert!(ActivityBursts::load(&path).sessions.is_empty());

        std::fs::write(&path, "{not json").unwrap();
        assert!(ActivityBursts::load(&path).sessions.is_empty());

        std::fs::write(&path, r#"{"sessions": {"a": {"confirmed": "text"}}}"#).unwrap();
        assert!(ActivityBursts::load(&path).sessions.is_empty());
    }

    #[test]
    fn epochs_round_trip_without_the_sampling_instant() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("activity_bursts.json");
        let mut bursts = ActivityBursts::default();
        bursts.sessions.insert(
            "a".to_string(),
            ActivityBurst {
                confirmed: 100,
                pending: Some((160, 160)),
                sampled_at: Some(Instant::now()),
            },
        );
        bursts.save(&path);

        let loaded = ActivityBursts::load(&path);
        let burst = loaded.sessions["a"];
        assert_eq!(burst.confirmed, 100);
        assert_eq!(burst.pending, Some((160, 160)));
        assert_eq!(burst.sampled_at, None);
    }

    #[test]
    fn observe_reports_persisted_changes_only() {
        let now = Instant::now();
        let mut bursts = ActivityBursts::default();
        assert!(bursts.observe("a", 100, now).changed);
        let repeat = bursts.observe("a", 100, now);
        assert!(!repeat.changed);
        assert_eq!(repeat.rewind_to, None);

        let lone = bursts.observe("a", 160, now);
        assert!(lone.changed);
        assert_eq!(lone.rewind_to, Some(100));
        let again = bursts.observe("a", 160, now);
        assert!(!again.changed);
        assert_eq!(again.rewind_to, Some(100));

        let confirmed = bursts.observe("a", 161, now);
        assert!(confirmed.changed);
        assert_eq!(confirmed.rewind_to, None);
        assert_eq!(bursts.sessions["a"].confirmed, 161);
    }

    #[test]
    fn retain_sessions_prunes_absent_entries() {
        let now = Instant::now();
        let mut bursts = ActivityBursts::default();
        bursts.observe("a", 1, now);
        bursts.observe("b", 1, now);
        let roster: HashSet<String> = HashSet::from(["a".to_string()]);
        assert_eq!(bursts.retain_sessions(&roster), 1);
        assert!(bursts.sessions.contains_key("a"));
        assert!(!bursts.sessions.contains_key("b"));
    }
}
