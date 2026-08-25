use std::collections::HashMap;
use std::path::Path;

use chrono::{Duration, Utc};

/// How many weeks to retain dismissed notification entries before pruning.
pub const NOTIFICATION_RETENTION_WEEKS: i64 = 3;

/// A single dismissed notification entry, recording when the user dismissed it
/// and the notification's `updated_at` at that moment. This allows detecting
/// new activity: if a later poll shows a newer `updated_at`, the thread should
/// be un-dismissed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DismissedEntry {
    pub dismissed_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DismissedNotifications {
    pub ids: HashMap<String, DismissedEntry>,
}

impl DismissedNotifications {
    pub fn load(path: &Path) -> Self {
        let data = match std::fs::read_to_string(path) {
            Ok(d) => d,
            Err(_) => return Self::empty(),
        };

        match serde_json::from_str::<Self>(&data) {
            Ok(dn) => dn,
            Err(_) => Self::empty(),
        }
    }

    pub fn save(&self, path: &Path) {
        if let Ok(data) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, data);
        }
    }

    /// Insert a dismissed entry, recording the notification's `updated_at` at the
    /// time of dismissal so we can detect new activity later.
    pub fn insert(&mut self, id: String, notification_updated_at: String) {
        let now = Utc::now().to_rfc3339();
        self.ids.insert(
            id,
            DismissedEntry {
                dismissed_at: now,
                updated_at: notification_updated_at,
            },
        );
    }

    pub fn contains(&self, id: &str) -> bool {
        self.ids.contains_key(id)
    }

    pub fn remove(&mut self, id: &str) -> bool {
        self.ids.remove(id).is_some()
    }

    /// Returns true if the notification should be un-dismissed because it has
    /// genuinely new activity: `is_unread` is true (GitHub marks it unread) AND
    /// `current_updated_at` is newer than what was stored at dismissal time.
    pub fn should_undismiss(
        &self,
        thread_id: &str,
        current_updated_at: &str,
        is_unread: bool,
    ) -> bool {
        if !is_unread {
            return false;
        }
        let entry = match self.ids.get(thread_id) {
            Some(e) => e,
            None => return false,
        };
        current_updated_at > entry.updated_at.as_str()
    }

    /// Remove entries older than `max_age`. Returns the number of entries pruned.
    pub fn prune_older_than(&mut self, max_age: Duration) -> usize {
        let cutoff = Utc::now() - max_age;
        let before = self.ids.len();
        self.ids.retain(|_id, entry| {
            chrono::DateTime::parse_from_rfc3339(&entry.dismissed_at)
                .map(|ts| ts >= cutoff)
                .unwrap_or(false) // remove entries with unparseable timestamps
        });
        let pruned = before - self.ids.len();
        if pruned > 0 {
            tracing::debug!(pruned, "pruned old dismissed notification entries");
        }
        pruned
    }

    fn empty() -> Self {
        Self {
            ids: HashMap::new(),
        }
    }
}
