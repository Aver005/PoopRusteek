//! Retrieval-quality harness for the skill matcher (nc-backend's
//! search-quality eval in miniature): a fixed corpus of realistic skills, a
//! fixed set of Russian/English queries with one expected skill each, MRR
//! as the aggregate metric.
//!
//! The `mrr_*` test needs the embedding model on disk (downloads ~120 MB on
//! the first run into the real `Config::data_dir()/models` cache, reused by
//! the app itself afterwards) — so it is `#[ignore]`d out of CI. Run
//! manually to calibrate `min_dense_score` / RRF changes:
//!
//! ```text
//! cargo test --bin pooprusteek semantic::eval -- --ignored --nocapture
//! ```

use super::matcher::SkillMatcher;
use crate::config::Config;
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

fn fixture_corpus() -> Vec<SkillDefinition> {
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
fn eval_queries() -> Vec<(&'static str, &'static str)> {
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

/// Mean Reciprocal Rank of the expected skill across all queries, printed
/// per-query so threshold tuning has something to look at.
#[test]
#[ignore = "needs the ~120 MB embedding model; run manually with --ignored --nocapture"]
fn mrr_over_fixture_corpus_is_acceptable() {
    let corpus = fixture_corpus();
    let cache_dir = Config::data_dir().join("models");
    let mut matcher =
        SkillMatcher::build(&corpus, cache_dir).expect("matcher build (model download?) failed");

    let queries = eval_queries();
    let mut reciprocal_ranks = Vec::new();
    println!("{:<55} {:<28} {:>5}", "query", "expected", "rank");
    for (query, expected) in &queries {
        let ranking = matcher.rank_all(query);
        let rank = ranking.iter().position(|slug| slug == expected);
        let rr = rank.map_or(0.0, |r| 1.0 / (r as f64 + 1.0));
        reciprocal_ranks.push(rr);
        println!(
            "{:<55} {:<28} {:>5}",
            query,
            expected,
            rank.map_or("—".to_string(), |r| (r + 1).to_string())
        );
    }
    let mrr: f64 = reciprocal_ranks.iter().sum::<f64>() / reciprocal_ranks.len() as f64;
    println!("MRR = {mrr:.3} over {} queries", queries.len());

    // e5-small + keyword sparse should place the right skill at #1 for the
    // clear majority of these; below 0.6 something regressed.
    assert!(mrr >= 0.6, "MRR degraded: {mrr:.3}");
}

/// Sanity check that needs no model: the fixture corpus builds a sparse
/// index where an exact keyword query wins.
#[test]
fn sparse_half_alone_finds_keyword_skills() {
    let corpus = fixture_corpus();
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
