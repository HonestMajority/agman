use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DismissedNotifications {
    pub ids: HashSet<String>,
}

impl DismissedNotifications {
    pub fn load(path: &Path) -> Self {
        if let Ok(data) = std::fs::read_to_string(path) {
            serde_json::from_str(&data).unwrap_or(Self {
                ids: HashSet::new(),
            })
        } else {
            Self {
                ids: HashSet::new(),
            }
        }
    }

    pub fn save(&self, path: &Path) {
        if let Ok(data) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, data);
        }
    }

    pub fn insert(&mut self, id: String) {
        self.ids.insert(id);
    }

    pub fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    pub fn remove(&mut self, id: &str) -> bool {
        self.ids.remove(id)
    }
}
