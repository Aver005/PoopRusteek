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
    if content.starts_with("---") {
        if let Some(end) = content[3..].find("---") {
            let fm_str = &content[3..3 + end];
            for line in fm_str.lines() {
                let line = line.trim();
                if let Some((key, value)) = line.split_once(':') {
                    let key = key.trim().to_lowercase();
                    let value = value.trim().trim_matches('"').trim_matches('\'').to_string();
                    match key.as_str() {
                        "name" => return SkillFrontmatter { name: Some(value), description: None },
                        "description" => return SkillFrontmatter { name: None, description: Some(value) },
                        _ => {}
                    }
                }
            }
        }
    }
    SkillFrontmatter { name: None, description: None }
}
