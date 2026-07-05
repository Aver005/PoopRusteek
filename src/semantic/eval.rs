//! Retrieval-quality harness for the semantic matcher (nc-backend's
//! search-quality eval in miniature): fixed corpora of realistic skills
//! and MCP tools, fixed Russian/English query sets with one expected
//! answer each, MRR as the aggregate metric.
//!
//! The `mrr_*` tests need the embedding model on disk (downloads ~120 MB
//! on the first run into the real `Config::data_dir()/models` cache,
//! reused by the app itself afterwards) — so they are `#[ignore]`d out of
//! CI. Run manually to calibrate `min_dense_score` / RRF changes:
//!
//! ```text
//! cargo test --bin pooprusteek semantic::eval -- --ignored --nocapture
//! ```

use super::embedder::Embedder;
use super::matcher::{McpCorpus, SkillCorpus};
use crate::config::Config;
use crate::mcp::manager::FullMCPTool;
use crate::mcp::types::MCPTool;
use crate::skills::{SkillDefinition, SkillSource};

fn skill(slug: &str, name: &str, description: &str) -> SkillDefinition {
    SkillDefinition {
        name: name.to_string(),
        slug: slug.to_string(),
        description: description.to_string(),
        source: SkillSource::Local,
        content: String::new(),
        enabled: false,
    }
}

fn fixture_skills() -> Vec<SkillDefinition> {
    vec![
        skill("code-review", "Code Review", "Review code changes for security, performance, and correctness before merging"),
        skill("security-review", "Security Review", "Audit code for vulnerabilities, injection risks, and unsafe patterns"),
        skill("rust-best-practices", "Rust Best Practices", "Guide for writing idiomatic Rust: ownership, borrowing, error handling with Result"),
        skill("tailwind", "Tailwind CSS", "Utility-first CSS styling, responsive layouts, and design tokens with Tailwind"),
        skill("playwright-expert", "Playwright Expert", "Browser automation and end-to-end testing with Playwright"),
        skill("telegram-bot", "Telegram Bot", "Build Telegram bots: commands, inline keyboards, webhooks"),
        skill("api-design-principles", "API Design Principles", "REST and GraphQL API design: resources, versioning, pagination, error contracts"),
        skill("frontend-design", "Frontend Design", "Create distinctive, production-grade web interfaces and landing pages"),
        skill("uv-package-manager", "UV Package Manager", "Python dependency management and virtual environments with uv"),
        skill("capacitor-push-notifications", "Capacitor Push Notifications", "Implement push notifications in mobile apps with FCM and APNs"),
        skill("zod", "Zod", "TypeScript schema validation and type-safe parsing with Zod"),
        skill("changelog-automation", "Changelog Automation", "Generate changelogs and release notes from commits and pull requests"),
    ]
}

/// (query, expected slug) — half Russian, half English, mixing exact
/// keyword hits with purely semantic phrasing.
fn skill_queries() -> Vec<(&'static str, &'static str)> {
    vec![
        ("сделай ревью этого пулл-реквеста перед мержем", "code-review"),
        ("проверь мой код на уязвимости и инъекции", "security-review"),
        ("как идиоматично обработать ошибки в расте", "rust-best-practices"),
        ("свёрстай адаптивную страницу на тейлвинде", "tailwind"),
        ("нужны e2e тесты в браузере", "playwright-expert"),
        ("напиши телеграм-бота с кнопками", "telegram-bot"),
        ("спроектируй REST API с пагинацией", "api-design-principles"),
        ("сделай красивый лендинг", "frontend-design"),
        ("настрой виртуальное окружение для питона", "uv-package-manager"),
        ("пуш-уведомления в мобильном приложении", "capacitor-push-notifications"),
        ("валидация данных по схеме в typescript", "zod"),
        ("собери release notes из коммитов", "changelog-automation"),
        ("review this diff for correctness", "code-review"),
        ("find vulnerabilities in my endpoint", "security-review"),
        ("style this component with utility classes", "tailwind"),
        ("automate the browser to click through the signup flow", "playwright-expert"),
    ]
}

fn mcp_tool(server: &str, name: &str, description: &str) -> FullMCPTool {
    FullMCPTool {
        full_name: format!("mcp__{server}__{name}"),
        tool: MCPTool {
            name: name.to_string(),
            description: description.to_string(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
            server_name: server.to_string(),
        },
    }
}

/// A realistic mixed catalog: browser automation + docs + filesystem-ish.
fn fixture_mcp_tools() -> Vec<FullMCPTool> {
    vec![
        mcp_tool("playwright", "browser_click", "Click an element on the current page"),
        mcp_tool("playwright", "browser_navigate", "Navigate the browser to a URL"),
        mcp_tool("playwright", "browser_take_screenshot", "Take a screenshot of the current page or element"),
        mcp_tool("playwright", "browser_fill_form", "Fill multiple form fields on the page"),
        mcp_tool("playwright", "browser_console_messages", "Read console messages from the page"),
        mcp_tool("playwright", "browser_network_requests", "List network requests made by the page"),
        mcp_tool("outline", "search_documents", "Full-text search across knowledge-base documents"),
        mcp_tool("outline", "create_document", "Create a new document in a collection"),
        mcp_tool("outline", "update_document", "Update an existing document's title or content"),
        mcp_tool("outline", "list_collections", "List all document collections"),
        mcp_tool("context7", "resolve_library_id", "Resolve a package or library name to a documentation library id"),
        mcp_tool("context7", "query_docs", "Fetch up-to-date documentation and code examples for a library"),
        mcp_tool("chrome", "performance_start_trace", "Start recording a performance trace of the page"),
        mcp_tool("chrome", "take_heapsnapshot", "Capture a V8 heap snapshot for memory analysis"),
        mcp_tool("chrome", "emulate", "Emulate device, network throttling, or CPU slowdown"),
    ]
}

fn mcp_queries() -> Vec<(&'static str, &'static str)> {
    vec![
        ("кликни по кнопке на странице", "mcp__playwright__browser_click"),
        ("сделай скриншот страницы", "mcp__playwright__browser_take_screenshot"),
        ("открой сайт в браузере", "mcp__playwright__browser_navigate"),
        ("заполни форму регистрации", "mcp__playwright__browser_fill_form"),
        ("что в консоли браузера, есть ли ошибки", "mcp__playwright__browser_console_messages"),
        ("найди документ в базе знаний", "mcp__outline__search_documents"),
        ("заведи новый документ", "mcp__outline__create_document"),
        ("свежая документация по библиотеке react", "mcp__context7__query_docs"),
        ("память течёт, нужен снапшот кучи", "mcp__chrome__take_heapsnapshot"),
        ("замедли сеть для теста", "mcp__chrome__emulate"),
        ("record a performance trace", "mcp__chrome__performance_start_trace"),
        ("what http calls did the page make", "mcp__playwright__browser_network_requests"),
    ]
}

fn print_row(query: &str, expected: &str, rank: Option<usize>) {
    println!(
        "{:<55} {:<40} {:>5}",
        query,
        expected,
        rank.map_or("—".to_string(), |r| (r + 1).to_string())
    );
}

fn mrr(reciprocal_ranks: &[f64]) -> f64 {
    reciprocal_ranks.iter().sum::<f64>() / reciprocal_ranks.len() as f64
}

#[test]
#[ignore = "needs the ~120 MB embedding model; run manually with --ignored --nocapture"]
fn mrr_over_skill_fixture_is_acceptable() {
    let skills = fixture_skills();
    let mut embedder = Embedder::init(Config::data_dir().join("models"))
        .expect("embedder init (model download?) failed");
    let corpus = SkillCorpus::build(&mut embedder, &skills).expect("corpus build failed");

    let queries = skill_queries();
    let mut reciprocal_ranks = Vec::new();
    for (query, expected) in &queries {
        let query_vec = embedder.embed_query(query).unwrap();
        let ranking = corpus.query(&query_vec, query, skills.len(), f32::MIN);
        let rank = ranking.iter().position(|m| m.slug == *expected);
        reciprocal_ranks.push(rank.map_or(0.0, |r| 1.0 / (r as f64 + 1.0)));
        print_row(query, expected, rank);
    }
    let mrr = mrr(&reciprocal_ranks);
    println!("skills MRR = {mrr:.3} over {} queries", queries.len());

    // e5-small + keyword sparse should place the right skill at #1 for the
    // clear majority of these; below 0.6 something regressed.
    assert!(mrr >= 0.6, "skills MRR degraded: {mrr:.3}");
}

#[test]
#[ignore = "needs the ~120 MB embedding model; run manually with --ignored --nocapture"]
fn mrr_over_mcp_tool_fixture_is_acceptable() {
    let tools = fixture_mcp_tools();
    let mut embedder = Embedder::init(Config::data_dir().join("models"))
        .expect("embedder init (model download?) failed");
    let corpus = McpCorpus::build(&mut embedder, &tools).expect("corpus build failed");

    let queries = mcp_queries();
    let mut reciprocal_ranks = Vec::new();
    for (query, expected) in &queries {
        let query_vec = embedder.embed_query(query).unwrap();
        let ranking = corpus.query(&query_vec, query, tools.len(), f32::MIN);
        let rank = ranking.iter().position(|m| m.full_name == *expected);
        reciprocal_ranks.push(rank.map_or(0.0, |r| 1.0 / (r as f64 + 1.0)));
        print_row(query, expected, rank);
    }
    let mrr = mrr(&reciprocal_ranks);
    println!("mcp MRR = {mrr:.3} over {} queries", queries.len());

    assert!(mrr >= 0.6, "mcp MRR degraded: {mrr:.3}");
}

/// Sanity check that needs no model: the fixture corpus builds a sparse
/// index where an exact keyword query wins.
#[test]
fn sparse_half_alone_finds_keyword_skills() {
    let corpus = fixture_skills();
    let texts: Vec<String> = corpus
        .iter()
        .map(|s| format!("{} ({}): {}", s.name, s.slug, s.description))
        .collect();
    let index = super::sparse::SparseIndex::build(&texts);
    let scores = index.scores("telegram bot with inline keyboards");
    let best = scores
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(i, _)| i)
        .unwrap();
    assert_eq!(corpus[best].slug, "telegram-bot");
}

/// The lexical `tool_search` fallback must work with no embedder at all —
/// that's what keeps deferred schemas from bricking when init fails.
#[test]
fn tool_search_lexical_fallback_works_without_embedder() {
    let service = super::SemanticService::disabled();
    service.update_mcp_tools(fixture_mcp_tools());
    let results = service.search_tools("click a button on the page", 3);
    assert!(!results.is_empty(), "lexical fallback returned nothing");
    assert_eq!(results[0].full_name, "mcp__playwright__browser_click");
}
