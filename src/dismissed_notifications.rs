use std::collections::HashMap;
use std::path::Path;

use chrono::{Duration, Utc};

/// How many weeks to retain dismissed notification entries before pruning.
pub const NOTIFICATION_RETENTION_WEEKS: i64 = 3;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DismissedNotifications {
    pub ids: HashMap<String, String>, // thread_id -> dismissed_at ISO 8601
}

/// Legacy format: `{ "ids": ["id1", "id2", ...] }` (HashSet<String>).
#[derive(serde::Deserialize)]
struct LegacyDismissedNotifications {
    ids: Vec<String>,
}

impl DismissedNotifications {
    pub fn load(path: &Path) -> Self {
        let data = match std::fs::read_to_string(path) {
            Ok(d) => d,
            Err(_) => return Self::empty(),
        };

        // Try new HashMap format first
        if let Ok(dn) = serde_json::from_str::<Self>(&data) {
            return dn;
        }

        // Fall back to legacy HashSet/Vec format — migrate entries with current timestamp
        if let Ok(legacy) = serde_json::from_str::<LegacyDismissedNotifications>(&data) {
            let now = Utc::now().to_rfc3339();
            let ids = legacy
                .ids
                .into_iter()
                .map(|id| (id, now.clone()))
                .collect();
            return Self { ids };
        }

        Self::empty()
    }

    pub fn save(&self, path: &Path) {
        if let Ok(data) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, data);
        }
    }

    pub fn insert(&mut self, id: String) {
        let now = Utc::now().to_rfc3339();
        self.ids.insert(id, now);
    }

    pub fn contains(&self, id: &str) -> bool {
        self.ids.contains_key(id)
    }

    pub fn remove(&mut self, id: &str) -> bool {
        self.ids.remove(id).is_some()
    }

    /// Remove entries older than `max_age`. Returns the number of entries pruned.
    pub fn prune_older_than(&mut self, max_age: Duration) -> usize {
        let cutoff = Utc::now() - max_age;
        let before = self.ids.len();
        self.ids.retain(|_id, dismissed_at| {
            chrono::DateTime::parse_from_rfc3339(dismissed_at)
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
