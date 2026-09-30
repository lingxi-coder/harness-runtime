//! In-memory skill registry with trigger-based discovery.
//!
//! See spec §18.2.

use crate::model::Skill;
use lingxi_core::types::{McpConnectionId, PluginId};
use std::collections::HashMap;

/// Stores all registered skills keyed by name and supports trigger-based
/// discovery against natural-language queries.
pub struct SkillRegistry {
    skills: HashMap<String, Skill>,
    trigger_index: HashMap<String, Vec<String>>,
    #[allow(dead_code)]
    mcp_skills: HashMap<McpConnectionId, Vec<String>>,
    plugin_skills: HashMap<PluginId, Vec<String>>,
}

impl SkillRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            skills: HashMap::new(),
            trigger_index: HashMap::new(),
            mcp_skills: HashMap::new(),
            plugin_skills: HashMap::new(),
        }
    }

    /// Register a skill, indexing its triggers (lowercased) for discovery.
    pub fn register(&mut self, skill: Skill) {
        for trig in &skill.frontmatter.triggers {
            self.trigger_index
                .entry(trig.to_lowercase())
                .or_default()
                .push(skill.name.clone());
        }
        self.skills.insert(skill.name.clone(), skill);
    }

    /// Look up a skill by canonical name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    /// All registered skill names (unsorted). Used by the `builtin` module and the
    /// composition-root snapshot tests to lock the assembled skill set.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.skills.keys().map(String::as_str).collect()
    }

    /// Number of registered skills.
    #[must_use]
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// True when no skills are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Return all skills whose triggers appear (substring match) in `query`.
    ///
    /// Match is case-insensitive on the trigger keyword. Each skill appears
    /// at most once even if several of its triggers fire.
    #[must_use]
    pub fn discover(&self, query: &str) -> Vec<&Skill> {
        let q = query.to_lowercase();
        let mut names: Vec<&String> = Vec::new();
        for (trig, skill_names) in &self.trigger_index {
            if q.contains(trig) {
                names.extend(skill_names);
            }
        }
        names.sort();
        names.dedup();
        names
            .into_iter()
            .filter_map(|n| self.skills.get(n))
            .collect()
    }

    /// Register a batch of skills owned by `plugin_id` and remember the
    /// owning plugin so [`Self::unregister_plugin`] can later clean them up.
    pub fn register_plugin_skills(&mut self, plugin_id: PluginId, skills: Vec<Skill>) {
        let names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
        for s in skills {
            self.register(s);
        }
        self.plugin_skills.insert(plugin_id, names);
    }

    /// Remove every skill previously registered under `plugin_id`.
    pub fn unregister_plugin(&mut self, plugin_id: &PluginId) {
        if let Some(names) = self.plugin_skills.remove(plugin_id) {
            for n in &names {
                self.skills.remove(n);
            }
        }
    }
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LoadedFrom, SkillFrontmatter, SkillSource};

    fn mk_skill(name: &str, triggers: &[&str]) -> Skill {
        Skill {
            name: name.into(),
            description: String::new(),
            frontmatter: SkillFrontmatter {
                name: name.into(),
                description: String::new(),
                triggers: triggers.iter().map(|s| (*s).into()).collect(),
                ..Default::default()
            },
            content: String::new(),
            source: SkillSource::Bundled,
            loaded_from: LoadedFrom::Bundled,
            plugin_id: None,
            file_path: "/tmp".into(),
        }
    }

    #[test]
    fn discover_matches_trigger() {
        let mut r = SkillRegistry::new();
        r.register(mk_skill("git-commit", &["commit", "git"]));
        let hits = r.discover("please commit my changes");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "git-commit");
    }
}
