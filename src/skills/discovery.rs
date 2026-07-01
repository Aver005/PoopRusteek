use super::{parse_skill_content, SkillDefinition, SkillSource};
use crate::config::Config;
use std::path::{Path, PathBuf};

const SKILL_FILE_NAME: &str = "SKILL.md";

fn home() -> Option<PathBuf> {
    dirs::home_dir()
}

fn config_dir() -> Option<PathBuf> {
    dirs::config_dir()
}

fn data_dir() -> Option<PathBuf> {
    dirs::data_dir()
}

/// Collect ALL candidate skill directories from every known agent location.
fn collect_skill_dirs() -> Vec<(PathBuf, SkillSource)> {
    let mut dirs: Vec<(PathBuf, SkillSource)> = Vec::new();
    let cwd = std::env::current_dir().ok();

    // ── Project-level ──
    if let Some(ref wd) = cwd {
        dirs.push((wd.join(".skills"), SkillSource::Local));
        dirs.push((wd.join(".claude").join("skills"), SkillSource::Local));
    }

    // ── User home agent directories ──
    if let Some(ref h) = home() {
        dirs.push((h.join(".claude").join("skills"), SkillSource::Installed));
        dirs.push((h.join(".agents").join("skills"), SkillSource::Installed));
        dirs.push((h.join(".agent").join("skills"), SkillSource::Installed));
        dirs.push((h.join(".windsurf").join("skills"), SkillSource::Installed));
        dirs.push((h.join(".cline").join("skills"), SkillSource::Installed));
        dirs.push((h.join(".cursor").join("skills"), SkillSource::Installed));
        dirs.push((h.join(".codex").join("skills"), SkillSource::Installed));
        dirs.push((h.join(".github").join("copilot").join("skills"), SkillSource::Installed));
    }

    // ── Config-level directories ──
    if let Some(ref cfg) = config_dir() {
        dirs.push((cfg.join("skills"), SkillSource::Installed));
        dirs.push((cfg.join("opencode").join("skills"), SkillSource::Installed));
        dirs.push((cfg.join("Claude").join("skills"), SkillSource::Installed));
        dirs.push((cfg.join("cursor").join("skills"), SkillSource::Installed));
        dirs.push((cfg.join("windsurf").join("skills"), SkillSource::Installed));
        dirs.push((cfg.join("cline").join("skills"), SkillSource::Installed));
        dirs.push((cfg.join("zed").join("skills"), SkillSource::Installed));
    }

    // ── Data-level directories ──
    if let Some(ref d) = data_dir() {
        dirs.push((d.join("skills"), SkillSource::Installed));
    }

    // ── Pooprusteek's own skills directory ──
    dirs.push((Config::data_dir().join("skills"), SkillSource::Installed));

    // ── Built-in prompts from assets/prompts/ ──
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent() {
            dirs.push((dir.join("assets").join("prompts"), SkillSource::BuiltIn));
        }
    let cargo_prompts = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets").join("prompts");
    if !dirs.iter().any(|(p, _)| p == &cargo_prompts) {
        dirs.push((cargo_prompts, SkillSource::BuiltIn));
    }

    dirs
}

pub fn discover_all_skills(config_paths: &[String]) -> Vec<SkillDefinition> {
    let mut skills = Vec::new();
    let mut seen_slugs = std::collections::HashSet::new();

    let mut base_dirs = collect_skill_dirs();

    for p in config_paths {
        let expanded = crate::util::expand_tilde(p);
        if expanded.exists() {
            base_dirs.push((expanded, SkillSource::Local));
        }
    }

    for (dir, source) in &base_dirs {
        if !dir.exists() || !dir.is_dir() {
            continue;
        }
        scan_directory(dir, *source, &mut skills, &mut seen_slugs);
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

fn scan_directory(
    dir: &Path,
    source: SkillSource,
    skills: &mut Vec<SkillDefinition>,
    seen_slugs: &mut std::collections::HashSet<String>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("Failed to read skill directory {}: {e}", dir.display());
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();

        if path.is_dir() {
            // Standard layout: subdir/SKILL.md (skill name = directory name)
            let skill_file = path.join(SKILL_FILE_NAME);
            if skill_file.exists() {
                if let Some(skill) = load_skill_from_file(&skill_file, source, &path)
                    && seen_slugs.insert(skill.slug.clone()) {
                        skills.push(skill);
                    }
                continue;
            }

            // Nested repo layout: owner-repo/skill-name/SKILL.md
            // Only recurse if this dir does NOT have SKILL.md but its children might
            if source != SkillSource::BuiltIn {
                let has_nested_skills = std::fs::read_dir(&path)
                    .ok()
                    .map(|e| {
                        e.flatten().any(|child| {
                            child.path().is_dir()
                                && child.path().join(SKILL_FILE_NAME).exists()
                        })
                    })
                    .unwrap_or(false);
                if has_nested_skills {
                    scan_directory(&path, source, skills, seen_slugs);
                    continue;
                }
            }
        }

        // Backward compat: .prompt.md files only in built-in dirs.
        // Skip base.prompt.md and tools.prompt.md — they are already loaded
        // by PromptFiles and injected directly into the system prompt.
        if source == SkillSource::BuiltIn
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
                && name.ends_with(".prompt.md")
                    && name != SKILL_FILE_NAME
                    && name != "base.prompt.md"
                    && name != "tools.prompt.md"
                    && let Some(skill) = load_prompt_file(&path, source)
                        && seen_slugs.insert(skill.slug.clone()) {
                            skills.push(skill);
                        }
    }
}

fn load_skill_from_file(path: &Path, source: SkillSource, skill_dir: &Path) -> Option<SkillDefinition> {
    let content = std::fs::read_to_string(path).ok()?;
    let frontmatter = parse_skill_content(path, &content);

    let dir_name = skill_dir.file_name()?.to_str()?;
    let slug = dir_name.to_lowercase().replace(' ', "-");

    let name = frontmatter.name.unwrap_or_else(|| {
        dir_name
            .replace(['-', '_'], " ")
            .split_whitespace()
            .map(|w| {
                let mut c = w.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    });

    let description = frontmatter.description.unwrap_or_else(|| format!("{source} skill: {name}"));

    Some(SkillDefinition {
        name,
        slug,
        description,
        source,
        content,
        enabled: false,
    })
}

fn load_prompt_file(path: &Path, source: SkillSource) -> Option<SkillDefinition> {
    let content = std::fs::read_to_string(path).ok()?;
    let filename = path.file_stem()?.to_str()?;

    let slug = filename
        .strip_suffix(".prompt")
        .unwrap_or(filename)
        .to_lowercase()
        .replace(' ', "-");

    let name = slug
        .split('-')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    let desc = format!("{source} skill: {name}");

    Some(SkillDefinition {
        name,
        slug,
        description: desc,
        source,
        content,
        enabled: false,
    })
}

pub fn load_enabled_skills_content(skills: &[SkillDefinition]) -> String {
    let enabled: Vec<_> = skills.iter().filter(|s| s.enabled).collect();
    if enabled.is_empty() {
        return String::new();
    }

    let mut sections = Vec::new();
    sections.push("\n\n---\n\n# Active Skills".to_string());

    for skill in &enabled {
        sections.push(format!(
            "\n## Skill: {}\n{}\n",
            skill.name,
            skill.content.trim()
        ));
    }

    sections.push("\n---\n".to_string());
    sections.join("\n")
}
