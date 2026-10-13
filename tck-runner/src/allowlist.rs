//! Allow-list of scenarios that are known not to pass, with reasons.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Why a scenario is on the allow-list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    /// The feature is valid openCypher but not implemented yet.
    NotImplemented,
    /// The scenario depends on a Neo4j-specific extension.
    Neo4jExtension,
    /// The feature is an explicit non-goal for grafile.
    OutOfScope,
}

/// Allow-list entry detail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Detail {
    /// Why the scenario does not pass.
    pub reason: Reason,
    /// Optional free-text note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The allow-list: per feature file, per scenario name. The special scenario
/// name `*` covers every scenario in the file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AllowList {
    /// Entries keyed by feature file path relative to the features directory.
    pub files: BTreeMap<String, BTreeMap<String, Detail>>,
}

impl AllowList {
    /// Load the allow-list, returning an empty list when the file does not
    /// exist.
    ///
    /// # Errors
    ///
    /// Returns an error when the file exists but cannot be read or parsed.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read allow-list `{}`", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("cannot parse allow-list `{}`", path.display()))
    }

    /// Write the allow-list as pretty JSON.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be written.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create `{}`", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self).context("cannot serialize allow-list")?;
        std::fs::write(path, format!("{text}\n"))
            .with_context(|| format!("cannot write allow-list `{}`", path.display()))
    }

    /// Look up the entry for a scenario, falling back to the file-level `*`
    /// entry.
    pub fn detail(&self, file: &str, scenario: &str) -> Option<&Detail> {
        let entries = self.files.get(file)?;
        entries.get(scenario).or_else(|| entries.get("*"))
    }

    /// Insert a scenario entry.
    pub fn insert(&mut self, file: &str, scenario: &str, detail: Detail) {
        self.files
            .entry(file.to_owned())
            .or_default()
            .insert(scenario.to_owned(), detail);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_covers_scenarios() {
        let mut list = AllowList::default();
        list.insert(
            "a/b.feature",
            "*",
            Detail {
                reason: Reason::Neo4jExtension,
                note: None,
            },
        );
        list.insert(
            "a/b.feature",
            "named",
            Detail {
                reason: Reason::OutOfScope,
                note: Some("why".into()),
            },
        );
        assert_eq!(
            list.detail("a/b.feature", "named").map(|d| d.reason),
            Some(Reason::OutOfScope)
        );
        assert_eq!(
            list.detail("a/b.feature", "other").map(|d| d.reason),
            Some(Reason::Neo4jExtension)
        );
        assert!(list.detail("other.feature", "x").is_none());
    }
}
