//! Domain corpora on top of [`HybridIndex`](super::index::HybridIndex):
//! the skill catalog and the MCP tool catalog. Dense matching catches
//! synonyms and cross-language queries (Russian prompt → English
//! description); the sparse half catches exact keyword hits (`tailwind`,
//! `playwright`, a tool name) that short descriptions under-represent
//! semantically.

use super::embedder::Embedder;
use super::index::HybridIndex;
use crate::mcp::manager::FullMCPTool;
use crate::skills::SkillDefinition;

// ── Skills ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SkillMatch {
    /// The `skill` tool accepts name or slug; hints use the slug — it is
    /// stable, lowercase, and what the registry test enforces.
    pub slug: String,
    pub description: String,
    pub dense: f32,
    pub sparse: f32,
}

struct SkillEntry {
    slug: String,
    description: String,
    enabled: bool,
}

pub struct SkillCorpus {
    entries: Vec<SkillEntry>,
    index: HybridIndex,
}

impl SkillCorpus {
    /// Embed the whole skill catalog. Blocking — call from `spawn_blocking`.
    pub fn build(embedder: &mut Embedder, skills: &[SkillDefinition]) -> Result<Self, String> {
        let corpus: Vec<String> = skills.iter().map(skill_corpus_text).collect();
        Ok(Self {
            entries: skills
                .iter()
                .map(|s| SkillEntry {
                    slug: s.slug.clone(),
                    description: s.description.clone(),
                    enabled: s.enabled,
                })
                .collect(),
            index: HybridIndex::build(embedder, &corpus)?,
        })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Ranked matches for an already-embedded prompt. Skills whose full
    /// text is already in the system prompt (`enabled`) are skipped —
    /// suggesting them would be noise.
    pub fn query(
        &self,
        query_vec: &[f32],
        prompt: &str,
        top_k: usize,
        min_dense: f32,
    ) -> Vec<SkillMatch> {
        self.index
            .query(query_vec, prompt, min_dense)
            .into_iter()
            .filter(|hit| !self.entries[hit.index].enabled)
            .take(top_k)
            .map(|hit| SkillMatch {
                slug: self.entries[hit.index].slug.clone(),
                description: self.entries[hit.index].description.clone(),
                dense: hit.dense,
                sparse: hit.sparse,
            })
            .collect()
    }
}

/// What the index sees for one skill. Name and slug carry the exact
/// keywords; the description carries the semantics.
fn skill_corpus_text(skill: &SkillDefinition) -> String {
    format!("{} ({}): {}", skill.name, skill.slug, skill.description)
}

// ── MCP tools ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct McpToolMatch {
    /// Fully-qualified callable name (`mcp__<server>__<tool>`).
    pub full_name: String,
    pub description: String,
    /// JSON schema of the tool's input — carried in the match so
    /// `tool_search` results and deferred-mode hints can show full
    /// definitions without touching the MCP manager (or its lock).
    pub schema: serde_json::Value,
    pub dense: f32,
    pub sparse: f32,
}

struct McpEntry {
    full_name: String,
    description: String,
    schema: serde_json::Value,
}

pub struct McpCorpus {
    entries: Vec<McpEntry>,
    index: HybridIndex,
}

impl McpCorpus {
    /// Embed the MCP tool catalog. Blocking — call from `spawn_blocking`.
    pub fn build(embedder: &mut Embedder, tools: &[FullMCPTool]) -> Result<Self, String> {
        let corpus: Vec<String> = tools.iter().map(mcp_corpus_text).collect();
        Ok(Self {
            entries: tools
                .iter()
                .map(|t| McpEntry {
                    full_name: t.full_name.clone(),
                    description: t.tool.description.clone(),
                    schema: t.tool.input_schema.clone(),
                })
                .collect(),
            index: HybridIndex::build(embedder, &corpus)?,
        })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn query(
        &self,
        query_vec: &[f32],
        prompt: &str,
        top_k: usize,
        min_dense: f32,
    ) -> Vec<McpToolMatch> {
        self.index
            .query(query_vec, prompt, min_dense)
            .into_iter()
            .take(top_k)
            .map(|hit| McpToolMatch {
                full_name: self.entries[hit.index].full_name.clone(),
                description: self.entries[hit.index].description.clone(),
                schema: self.entries[hit.index].schema.clone(),
                dense: hit.dense,
                sparse: hit.sparse,
            })
            .collect()
    }
}

/// The `mcp__server__tool` name tokenizes into server + tool words for the
/// sparse half; the description carries the semantics for the dense half.
fn mcp_corpus_text(tool: &FullMCPTool) -> String {
    format!("{}: {}", tool.full_name, tool.tool.description)
}
