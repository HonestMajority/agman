use std::collections::HashMap;
use std::path::Path;

/// Per-session "last time a tmux client had this window on screen", in unix
/// epoch seconds. Backs the panel's "unseen" hint: an idle agent whose window
/// produced output after this stamp probably needs a look. This is a
/// needs-attention heuristic, not a semantic completion signal.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LastViewed {
    /// Epoch at which tracking started (file creation). Sessions without an
    /// explicit stamp fall back to this so a fresh install does not flag
    /// every pre-existing idle agent as unseen.
    pub tracking_since: i64,
    pub sessions: HashMap<String, i64>,
}

impl LastViewed {
    pub fn load(path: &Path, now_epoch: i64) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|data| serde_json::from_str::<Self>(&data).ok())
            .unwrap_or_else(|| Self::empty(now_epoch))
    }

    pub fn save(&self, path: &Path) {
        if let Ok(data) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, data);
        }
    }

    pub fn epoch_for(&self, session: &str) -> i64 {
        self.sessions
            .get(session)
            .copied()
            .unwrap_or(self.tracking_since)
    }

    /// Record `session` as viewed at `epoch`. Stamps never move backwards.
    /// Returns true when the stored value changed.
    pub fn stamp(&mut self, session: &str, epoch: i64) -> bool {
        match self.sessions.get_mut(session) {
            Some(current) if *current >= epoch => false,
            Some(current) => {
                *current = epoch;
                true
            }
            None => {
                self.sessions.insert(session.to_string(), epoch);
                true
            }
        }
    }

    /// Drop stamps for sessions no longer in the roster (archived agents,
    /// deleted projects). Returns the number of entries pruned.
    pub fn retain_sessions(&mut self, roster: &std::collections::HashSet<String>) -> usize {
        let before = self.sessions.len();
        self.sessions.retain(|session, _| roster.contains(session));
        let pruned = before - self.sessions.len();
        if pruned > 0 {
            tracing::debug!(pruned, "pruned last-viewed entries for absent sessions");
        }
        pruned
    }

    fn empty(now_epoch: i64) -> Self {
        Self {
            tracking_since: now_epoch,
            sessions: HashMap::new(),
        }
    }
}
