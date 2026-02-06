use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RepoStats {
    pub counts: HashMap<String, u64>,
}

impl RepoStats {
    pub fn load(path: &Path) -> Self {
        if let Ok(data) = std::fs::read_to_string(path) {
            serde_json::from_str(&data).unwrap_or(Self {
                counts: HashMap::new(),
            })
        } else {
            Self {
                counts: HashMap::new(),
            }
        }
    }

    pub fn save(&self, path: &Path) {
        if let Ok(data) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, data);
        }
    }

    pub fn increment(&mut self, repo_name: &str) {
        *self.counts.entry(repo_name.to_string()).or_insert(0) += 1;
    }

    /// Return repos sorted by count descending, only repos with count > 0
    pub fn favorites(&self) -> Vec<(String, u64)> {
        let mut entries: Vec<(String, u64)> = self
            .counts
            .iter()
            .filter(|(_, &count)| count > 0)
            .map(|(name, &count)| (name.clone(), count))
            .collect();
        entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        entries
    }
}
