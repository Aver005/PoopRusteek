pub mod discovery;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum SkillSource {
    BuiltIn,
    Local,
    Installed,
}

impl std::fmt::Display for SkillSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillSource::BuiltIn => write!(f, "built-in"),
            SkillSource::Local => write!(f, "local"),
            SkillSource::Installed => write!(f, "installed"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDefinition {
    pub name: String,
    pub slug: String,
    pub description: String,
    pub source: SkillSource,
    pub content: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillFrontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
}

pub fn parse_skill_content(_path: &std::path::Path, content: &str) -> SkillFrontmatter {
    let content = content.trim();
    let mut frontmatter = SkillFrontmatter {
        name: None,
        description: None,
    };

    if let Some(after_open) = content.strip_prefix("---")
        && let Some(end) = after_open.find("---")
    {
        let fm_str = &after_open[..end];
        // Accumulate every recognized key across all frontmatter lines
        // instead of returning on the first match — a file with both
        // `name:` and `description:` used to silently drop whichever
        // key came second.
        for line in fm_str.lines() {
            let line = line.trim();
            if let Some((key, value)) = line.split_once(':') {
                let key = key.trim().to_lowercase();
                let value = value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string();
                match key.as_str() {
                    "name" => frontmatter.name = Some(value),
                    "description" => frontmatter.description = Some(value),
                    _ => {}
                }
            }
        }
    }

    frontmatter
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn both_keys_are_kept_name_then_description() {
        let content = "---\nname: My Skill\ndescription: Does a thing\n---\nBody text.";
        let fm = parse_skill_content(Path::new("SKILL.md"), content);
        assert_eq!(fm.name.as_deref(), Some("My Skill"));
        assert_eq!(fm.description.as_deref(), Some("Does a thing"));
    }

    #[test]
    fn both_keys_are_kept_description_then_name() {
        let content = "---\ndescription: Does a thing\nname: My Skill\n---\nBody text.";
        let fm = parse_skill_content(Path::new("SKILL.md"), content);
        assert_eq!(fm.name.as_deref(), Some("My Skill"));
        assert_eq!(fm.description.as_deref(), Some("Does a thing"));
    }

    #[test]
    fn quotes_are_stripped_from_values() {
        let content = "---\nname: \"Quoted Name\"\ndescription: 'Single quoted'\n---\n";
        let fm = parse_skill_content(Path::new("SKILL.md"), content);
        assert_eq!(fm.name.as_deref(), Some("Quoted Name"));
        assert_eq!(fm.description.as_deref(), Some("Single quoted"));
    }

    #[test]
    fn unknown_keys_are_ignored_without_dropping_known_ones() {
        let content = "---\nfoo: bar\nname: My Skill\nbaz: qux\ndescription: Does a thing\n---\n";
        let fm = parse_skill_content(Path::new("SKILL.md"), content);
        assert_eq!(fm.name.as_deref(), Some("My Skill"));
        assert_eq!(fm.description.as_deref(), Some("Does a thing"));
    }

    #[test]
    fn no_frontmatter_returns_empty() {
        let fm = parse_skill_content(Path::new("SKILL.md"), "Just body text, no frontmatter.");
        assert!(fm.name.is_none());
        assert!(fm.description.is_none());
    }
}
