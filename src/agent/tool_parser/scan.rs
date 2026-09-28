//! Один проход по ответу модели слева направо: вызовы всех форматов, блоки
//! кода и рассуждения — в порядке текста.

use super::accept::{self, Judged};
use super::formats::{self, Found};
use super::{ParseCtx, ParsedReply};
use regex::{Captures, Regex};
use std::ops::Range;
use std::sync::LazyLock;

const CALL_IN_REASONING: &str = "a tool call was written inside a <thinking> block, so it \
     was not run. Write the call outside the thinking block.";

#[derive(Clone, Copy)]
enum Token {
    Fence,
    Inline,
    ThinkOpen,
    ThinkClose,
    Call,
}

struct Scanner {
    re: Regex,
    /// Номер группы захвата → что нашлось.
    groups: Vec<(usize, Token)>,
    families: Vec<formats::Family>,
    /// Маркер каждого семейства, привязанный к началу: на одной позиции
    /// пробуются все совпавшие.
    anchored: Vec<Regex>,
}

static SCANNER: LazyLock<Scanner> = LazyLock::new(Scanner::new);
static FENCE_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^[ \t]*```").expect("hardcoded regex is valid"));
static THINK_CLOSE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"</think(?:ing)?>").expect("hardcoded regex is valid"));
static BACKTICKS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"`+").expect("hardcoded regex is valid"));
static NAMED_GROUP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\(\?P?<[A-Za-z_][A-Za-z0-9_]*>").expect("hardcoded regex is valid")
});

impl Scanner {
    fn new() -> Self {
        let families = formats::families();
        let mut parts = vec![
            (Token::Fence, r"(?m:^)[ \t]*```".to_string()),
            (Token::Inline, "`+".to_string()),
            (Token::ThinkOpen, "<think(?:ing)?>".to_string()),
            (Token::ThinkClose, "</think(?:ing)?>".to_string()),
        ];
        // Именованные группы маркеров сканеру не нужны, а одинаковые имена у
        // двух семейств не дали бы собрать общую регулярку.
        parts.extend(families.iter().map(|family| {
            (
                Token::Call,
                NAMED_GROUP_RE
                    .replace_all(&family.opener, "(?:")
                    .into_owned(),
            )
        }));
        let anchored = families
            .iter()
            .map(|family| {
                Regex::new(&format!("^(?:{})", family.opener))
                    .expect("family openers are valid regexes")
            })
            .collect();
        let pattern = parts
            .iter()
            .enumerate()
            .map(|(i, (_, part))| format!("(?P<t{i}>{part})"))
            .collect::<Vec<_>>()
            .join("|");
        let re = Regex::new(&pattern).expect("family openers are valid regexes");
        let groups = parts
            .iter()
            .enumerate()
            .map(|(i, (token, _))| {
                let name = format!("t{i}");
                let index = re
                    .capture_names()
                    .position(|group| group == Some(name.as_str()))
                    .expect("every part has its named group");
                (index, *token)
            })
            .collect();
        Self {
            re,
            groups,
            families,
            anchored,
        }
    }

    /// Разобрать вызов с позиции `at`. Один маркер (`<tool_call>`,
    /// `<function_calls>`) бывает у нескольких форматов: каждый сам решает,
    /// его ли тело, и первый согласившийся забирает находку.
    fn parse_at<'s>(&'s self, hay: &'s str, at: usize) -> impl Iterator<Item = Found> + 's {
        let rest = &hay[at..];
        self.families
            .iter()
            .zip(&self.anchored)
            .filter(move |(_, opener)| opener.is_match(rest))
            .filter_map(move |(family, _)| (family.parse)(hay, at))
    }

    fn token(&self, caps: &Captures) -> Token {
        self.groups
            .iter()
            .find(|(index, _)| caps.get(*index).is_some())
            .map(|(_, token)| *token)
            .expect("exactly one alternative matched")
    }

    /// Ближайший маркер формата с позиции `from`, за которым есть тело вызова.
    fn next_call(&self, hay: &str, from: usize) -> Option<Found> {
        let mut pos = from;
        while let Some(caps) = self.re.captures_at(hay, pos) {
            let m = caps.get(0)?;
            if let Token::Call = self.token(&caps)
                && let Some(found) = self.parse_at(hay, m.start()).next()
            {
                return Some(found);
            }
            pos = m.end();
        }
        None
    }
}

#[derive(Default)]
struct Pass {
    found: Vec<Judged>,
    /// Куски, которых нет в показанном ответе.
    cut: Vec<Range<usize>>,
    calls_in_reasoning: bool,
}

impl Pass {
    fn has_calls(&self) -> bool {
        self.found
            .iter()
            .any(|found| found.items.iter().any(Result::is_ok))
    }

    fn take(&mut self, found: Judged) -> usize {
        let end = found.span.end;
        self.cut.push(found.span.clone());
        self.found.push(found);
        end
    }

    /// Принять находки вложенного прохода вместе с куском, который они занимают.
    fn absorb(&mut self, sub: Pass, span: Range<usize>) -> usize {
        let end = sub
            .found
            .iter()
            .map(|found| found.span.end)
            .fold(span.end, usize::max);
        self.found.extend(sub.found);
        self.calls_in_reasoning |= sub.calls_in_reasoning;
        self.cut.push(span.start..end);
        end
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    /// Остановиться на строке ``` вне находок и вернуть её.
    UntilFence,
}

pub(super) fn run(text: &str, ctx: &ParseCtx) -> ParsedReply {
    let mut pass = Pass::default();
    scan(text, 0..text.len(), &mut pass, Mode::Normal, ctx);
    let (mut calls, mut errors) = accept::collect(pass.found);
    let mut visible = visible(text, pass.cut);
    // Слабый формат (pythonic, голый JSON) — только весь ответ целиком и
    // только когда сильных находок нет: иначе это пример в объяснении.
    if calls.is_empty()
        && errors.is_empty()
        && let Some(raws) = formats::whole_reply(&visible)
        && let Some(items) = accept::judge_weak(&visible, raws, ctx)
    {
        for item in items {
            match item {
                Ok(call) => calls.push(call),
                Err(error) => errors.push(error),
            }
        }
        visible.clear();
    }
    if calls.is_empty() && pass.calls_in_reasoning {
        errors.push(CALL_IN_REASONING.to_string());
    }
    let suspect = accept::residue(&visible, ctx);
    ParsedReply {
        calls,
        errors,
        visible,
        suspect,
    }
}

/// Пройти `range`. В режиме `UntilFence` вернуть строку ```, на которой
/// проход остановился.
fn scan(
    text: &str,
    range: Range<usize>,
    pass: &mut Pass,
    mode: Mode,
    ctx: &ParseCtx,
) -> Option<Range<usize>> {
    let hay = &text[..range.end];
    let mut pos = range.start;
    while let Some(caps) = SCANNER.re.captures_at(hay, pos) {
        let m = caps.get(0).expect("group 0 is the whole match");
        pos = match SCANNER.token(&caps) {
            Token::Fence if mode == Mode::UntilFence => return Some(m.range()),
            Token::Fence => fence(text, hay, m.range(), pass, ctx),
            Token::Inline => inline_code(hay, m.range()),
            Token::ThinkOpen => reasoning(text, hay, m.range(), pass, ctx),
            Token::ThinkClose => bare_close(range.start, m.range(), pass),
            // Находка, от которой после приёмки ничего не осталось, — проза.
            Token::Call => {
                // Разобранный, но отвергнутый приёмкой вызов — проза целиком:
                // его аргументы не разбираются чужими грамматиками заново.
                let mut prose_end = m.end();
                let judged = SCANNER.parse_at(hay, m.start()).find_map(|found| {
                    let end = found.span.end;
                    let judged = accept::judge(text, found, ctx);
                    if judged.is_none() {
                        prose_end = prose_end.max(end);
                    }
                    judged
                });
                match judged {
                    Some(judged) => pass.take(judged).max(m.end()),
                    None => prose_end,
                }
            }
        };
    }
    None
}

/// Блок кода: вызовы из него берутся, только если в теле нет ничего, кроме
/// них. Иначе это пример, и он остаётся текстом.
fn fence(text: &str, hay: &str, open: Range<usize>, pass: &mut Pass, ctx: &ParseCtx) -> usize {
    let body_from = line_end(hay, open.end);
    let naive = FENCE_LINE_RE.find_at(hay, body_from).map(|m| m.range());
    let naive_end = naive.as_ref().map_or(hay.len(), |close| close.start);
    let naive_close = naive
        .as_ref()
        .map_or(hay.len(), |close| line_end(hay, close.end));
    let mut sub = Pass::default();
    scan(text, body_from..naive_end, &mut sub, Mode::Normal, ctx);
    let naive_calls = only_calls(hay, &sub, body_from..naive_end);
    // Вызов, оборванный ровно на строке ```, мог нести её в своём аргументе.
    let cut_by_fence = naive.is_some()
        && sub
            .found
            .iter()
            .any(|found| found.span.end >= naive_end && found.items.iter().any(Result::is_err));
    if naive_calls && !cut_by_fence {
        return pass.absorb(sub, open.start..naive_close);
    }
    if naive.is_none() {
        // Незакрытый блок с текстом вокруг вызова — недописанная разметка, а
        // не пример: вызовы в нём настоящие.
        return body_from;
    }
    let mut through = Pass::default();
    if let Some(close) = scan(
        text,
        body_from..hay.len(),
        &mut through,
        Mode::UntilFence,
        ctx,
    ) && only_calls(hay, &through, body_from..close.start)
    {
        return pass.absorb(through, open.start..line_end(hay, close.end));
    }
    if naive_calls {
        return pass.absorb(sub, open.start..naive_close);
    }
    naive_close
}

fn only_calls(hay: &str, sub: &Pass, body: Range<usize>) -> bool {
    if sub.found.is_empty() {
        return false;
    }
    let mut cut = sub.cut.clone();
    cut.sort_by_key(|span| span.start);
    let mut pos = body.start;
    for span in cut {
        if span.start > pos && !hay[pos..span.start.min(body.end)].trim().is_empty() {
            return false;
        }
        pos = pos.max(span.end);
    }
    pos >= body.end || hay[pos..body.end].trim().is_empty()
}

/// Встроенный код — проза. Но если закрывающая кавычка оказалась внутри
/// вызова (экранирование в PowerShell), кода нет: это начало вызова.
fn inline_code(hay: &str, open: Range<usize>) -> usize {
    let run = open.len();
    let line = hay[open.end..]
        .find('\n')
        .map_or(hay.len(), |offset| open.end + offset);
    let Some(close) = BACKTICKS_RE
        .find_iter(&hay[open.end..line])
        .find(|m| m.len() == run)
        .map(|m| open.end + m.start())
    else {
        return open.end;
    };
    let inside = &hay[..close];
    let mut pos = open.end;
    while let Some(caps) = SCANNER.re.captures_at(inside, pos) {
        let m = caps.get(0).expect("group 0 is the whole match");
        // Только целый вызов: битая находка тянется до конца текста и
        // «перекрыла» бы любой процитированный маркер.
        if let Token::Call = SCANNER.token(&caps)
            && SCANNER
                .parse_at(hay, m.start())
                .next()
                .is_some_and(|found| {
                    found.span.end > close && found.items.iter().any(Result::is_ok)
                })
        {
            return open.end;
        }
        pos = m.end();
    }
    close + run
}

/// Рассуждения не разбираются. Незакрытое кончается на первом настоящем
/// вызове: модель забыла закрыть тег, а не спрятала вызов.
fn reasoning(text: &str, hay: &str, open: Range<usize>, pass: &mut Pass, ctx: &ParseCtx) -> usize {
    let (body_end, end) = match THINK_CLOSE_RE.find_at(hay, open.end) {
        Some(close) => (close.start(), close.end()),
        None => {
            let end = SCANNER
                .next_call(hay, open.end)
                .map_or(hay.len(), |found| found.span.start);
            (end, end)
        }
    };
    let mut sub = Pass::default();
    scan(text, open.end..body_end, &mut sub, Mode::Normal, ctx);
    pass.calls_in_reasoning |= sub.has_calls() || sub.calls_in_reasoning;
    pass.cut.push(open.start..end);
    end
}

/// Закрывающий тег без открывающего: рассуждение открыл шаблон модели, и
/// всё до тега — оно. Если вызовы уже были, убирается только тег.
fn bare_close(start: usize, close: Range<usize>, pass: &mut Pass) -> usize {
    let from = if pass.found.is_empty() {
        start
    } else {
        close.start
    };
    pass.cut.push(from..close.end);
    close.end
}

fn line_end(hay: &str, pos: usize) -> usize {
    hay[pos..]
        .find('\n')
        .map_or(hay.len(), |offset| pos + offset + 1)
}

fn visible(text: &str, mut cut: Vec<Range<usize>>) -> String {
    cut.sort_by_key(|span| span.start);
    let mut shown = String::with_capacity(text.len());
    let mut pos = 0;
    for span in cut {
        if span.start > pos {
            push_piece(&mut shown, &text[pos..span.start]);
        }
        pos = pos.max(span.end);
    }
    if pos < text.len() {
        push_piece(&mut shown, &text[pos..]);
    }
    shown.trim().to_string()
}

/// На шве вырезанного вызова больше одной пустой строки не остаётся: V4.1
/// отделяет блок вызовов `\n\n` с обеих сторон.
fn push_piece(shown: &mut String, piece: &str) {
    let left = shown.len() - shown.trim_end_matches('\n').len();
    let lead = piece.len() - piece.trim_start_matches('\n').len();
    let keep = lead.min(2usize.saturating_sub(left));
    shown.push_str(&piece[lead - keep..]);
}

#[cfg(test)]
mod tests {
    use super::super::parse_reply;

    const CALL: &str =
        "<tool_use><name>bash</name><arguments>{\"command\":\"ls\"}</arguments></tool_use>";

    #[test]
    fn a_fence_holding_only_a_call_runs_it_and_hides_the_fence() {
        let reply = parse_reply(&format!("Listing.\n```xml\n{CALL}\n```\n"));
        assert_eq!(reply.calls.len(), 1);
        assert_eq!(reply.visible, "Listing.");
    }

    /// Пример в блоке кода рядом с пояснением — текст, а не вызов.
    #[test]
    fn a_fence_with_prose_around_a_call_is_an_example() {
        let text = format!("Like this:\n```\n# how a call looks\n{CALL}\n```");
        let reply = parse_reply(&text);
        assert!(reply.calls.is_empty() && reply.errors.is_empty());
        assert_eq!(reply.visible, text);
    }

    /// Запись markdown со своими ``` внутри блока ```xml не рвёт вызов.
    #[test]
    fn a_call_whose_argument_holds_fences_survives_its_own_fence() {
        let text = "```xml\n<｜DSML｜invoke name=\"write\"><｜DSML｜parameter name=\"content\" string=\"true\">a\n```\nb\n```\n</｜DSML｜parameter></｜DSML｜invoke>\n```\nDone.";
        let reply = parse_reply(text);
        assert!(reply.errors.is_empty(), "{:?}", reply.errors);
        assert_eq!(reply.calls.len(), 1);
        assert_eq!(reply.calls[0].arguments["content"], "a\n```\nb\n```\n");
        assert_eq!(reply.visible, "Done.");
    }

    /// Промпт велит остановиться после вызова — блок остаётся незакрытым.
    #[test]
    fn an_unclosed_fence_after_a_call_still_runs_it() {
        let reply = parse_reply(&format!("```xml\n{CALL}"));
        assert_eq!(reply.calls.len(), 1);
        assert_eq!(reply.visible, "");
        let reply = parse_reply(&format!("```xml\n{CALL}\nWaiting for the result."));
        assert_eq!(reply.calls.len(), 1);
    }

    /// Оборванный вызов в закрытом блоке — ошибка, но текст за блоком цел.
    #[test]
    fn a_broken_call_in_a_fence_keeps_the_text_after_it() {
        let reply = parse_reply("```xml\n<tool_use><name>bash</name>\n```\nMore prose.");
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1);
        assert_eq!(reply.visible, "More prose.");
    }

    #[test]
    fn a_call_quoted_in_inline_code_is_prose() {
        let text = format!("Calls look like `{CALL}` in this repo.");
        let reply = parse_reply(&text);
        assert!(reply.calls.is_empty());
        assert_eq!(reply.visible, text);
    }

    /// Процитированный битый маркер во встроенном коде — проза, а не ошибка:
    /// замер по документации проекта дал здесь ложный повтор.
    #[test]
    fn a_broken_marker_quoted_in_inline_code_is_prose() {
        let text = "Jamba writes `<tool_calls>[…]</tool_calls>` and stops.";
        let reply = parse_reply(text);
        assert!(
            reply.calls.is_empty() && reply.errors.is_empty(),
            "{:?}",
            reply.errors
        );
        assert_eq!(reply.visible, text);
    }

    /// Обратная кавычка PowerShell в аргументе не прячет вызов за «кодом».
    #[test]
    fn a_powershell_backtick_does_not_hide_the_call() {
        let text = "Run `it`: `<tool_use><name>powershell</name><arguments>{\"command\":\"Write-Host a`tb\"}</arguments></tool_use>";
        let reply = parse_reply(text);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments["command"], "Write-Host a`tb");
    }

    #[test]
    fn a_call_inside_thinking_is_reported_not_run() {
        let reply = parse_reply(&format!("<thinking>maybe {CALL}</thinking>I'll check."));
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1);
        assert_eq!(reply.visible, "I'll check.");
    }

    /// Незакрытое рассуждение кончается на настоящем вызове.
    #[test]
    fn an_unclosed_thinking_ends_at_the_first_real_call() {
        let reply = parse_reply(&format!("<thinking>plan: list files\n{CALL}"));
        assert_eq!(reply.calls.len(), 1);
        assert!(reply.errors.is_empty());
        assert_eq!(reply.visible, "");
    }

    #[test]
    fn a_bare_closing_think_hides_what_came_before() {
        let reply = parse_reply("draft reasoning</think>Answer.");
        assert_eq!(reply.visible, "Answer.");
    }

    #[test]
    fn a_closing_tag_inside_a_string_keeps_the_whole_value() {
        let text = "<tool_use><name>write</name><arguments>{\"content\":\"end with </tool_use> and </arguments>\"}</arguments></tool_use>";
        let reply = parse_reply(text);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(
            reply.calls[0].arguments["content"],
            "end with </tool_use> and </arguments>"
        );
    }

    /// Битый внешний вызов: вложенный в его аргумент — данные, не вызов.
    #[test]
    fn a_call_inside_a_broken_call_never_runs() {
        let text = "<tool_use><name>write</name><arguments>{\"content\":\"x \"bad\" <｜DSML｜invoke name=\"bash\"><｜DSML｜parameter name=\"command\" string=\"true\">rm -rf x</｜DSML｜parameter></｜DSML｜invoke>\"}</arguments></tool_use>";
        let reply = parse_reply(text);
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1);
    }

    #[test]
    fn a_call_cut_off_mid_json_is_reported() {
        let reply =
            parse_reply("Checking.\n<tool_use><name>bash</name><arguments>{\"command\":\"l");
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1);
        assert_eq!(reply.visible, "Checking.");
    }

    #[test]
    fn legacy_arguments_may_nest_objects() {
        let reply = parse_reply("[TOOL:edit] {\"opts\": {\"dry\": true}, \"path\": \"a\"}");
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments["opts"]["dry"], true);
    }

    /// Повтор не подряд — законный: тест, правка, тот же тест.
    #[test]
    fn only_an_adjacent_echo_is_dropped() {
        let dsml_ls = "<｜DSML｜invoke name=\"bash\"><｜DSML｜parameter name=\"command\" string=\"true\">ls</｜DSML｜parameter></｜DSML｜invoke>";
        let dsml_pwd = dsml_ls.replace(">ls<", ">pwd<");
        let reply = parse_reply(&format!("{CALL}{dsml_pwd}{dsml_ls}"));
        assert_eq!(reply.calls.len(), 3);
    }

    /// Чужой вызов незнакомого инструмента — проза целиком: доверенный
    /// `<tool_use>` в его аргументе раньше выполнялся без подтверждения.
    #[test]
    fn a_call_inside_a_rejected_foreign_call_never_runs() {
        let text = format!(
            "<function_calls><invoke name=\"nope\"><parameter name=\"doc\">{CALL}</parameter></invoke></function_calls>"
        );
        let reply = super::super::parse_with(&text, &[("bash", serde_json::json!({}))]);
        assert!(reply.calls.is_empty(), "{:?}", reply.calls);
        assert!(reply.errors.is_empty(), "{:?}", reply.errors);
    }

    /// Образец каждого семейства; вызываемое имя уникально для образца.
    const SAMPLES: &[(&str, &str)] = &[
        (
            "t_tool_use",
            "<tool_use><name>t_tool_use</name><arguments>{\"x\": 1}</arguments></tool_use>",
        ),
        ("t_legacy", "[TOOL:t_legacy] {\"x\": 1}"),
        (
            "t_dsml",
            "<｜DSML｜ calls>\n<｜DSML｜ invoke name=\"t_dsml\">\n<｜DSML｜ parameter name=\"x\" string=\"false\">1</｜DSML｜ parameter>\n</｜DSML｜ invoke>\n</｜DSML｜ calls>",
        ),
        (
            "t_v31",
            "<｜tool▁calls▁begin｜><｜tool▁call▁begin｜>t_v31<｜tool▁sep｜>{\"x\": 1}<｜tool▁call▁end｜><｜tool▁calls▁end｜>",
        ),
        (
            "t_claude",
            "<function_calls><invoke name=\"t_claude\"><parameter name=\"x\">1</parameter></invoke></function_calls>",
        ),
        (
            "t_hermes",
            "<tool_call>{\"name\": \"t_hermes\", \"arguments\": {\"x\": 1}}</tool_call>",
        ),
        (
            "t_glm",
            "<tool_call>t_glm\n<arg_key>x</arg_key>\n<arg_value>1</arg_value>\n</tool_call>",
        ),
        (
            "t_qwen",
            "<tool_call>\n<function=t_qwen>\n<parameter=x>\n1\n</parameter>\n</function>\n</tool_call>",
        ),
        (
            "t_mistral",
            "[TOOL_CALLS] [{\"name\": \"t_mistral\", \"arguments\": {\"x\": 1}}]",
        ),
        (
            "t_kimi",
            "<|tool_calls_section_begin|><|tool_call_begin|>functions.t_kimi:0<|tool_call_argument_begin|>{\"x\": 1}<|tool_call_end|><|tool_calls_section_end|>",
        ),
        (
            "t_harmony",
            "<|channel|>commentary to=functions.t_harmony <|constrain|>json<|message|>{\"x\": 1}<|call|>",
        ),
        ("t_gemma", "<|tool_call>call:t_gemma{x:1}<tool_call|>"),
        ("t_lfm", "<|tool_call_start|>[t_lfm(x=1)]<|tool_call_end|>"),
    ];

    /// Любые два формата в одном ответе: оба вызова, в порядке текста.
    #[test]
    fn every_pair_of_formats_mixes_in_text_order() {
        let schema =
            serde_json::json!({"type": "object", "properties": {"x": {"type": "integer"}}});
        let tools: Vec<_> = SAMPLES
            .iter()
            .map(|(name, _)| (*name, schema.clone()))
            .collect();
        let mut failures = Vec::new();
        for (first, a) in SAMPLES {
            for (second, b) in SAMPLES.iter().filter(|(name, _)| name != first) {
                let text = format!("Step one.\n{a}\nThen step two.\n{b}\nDone.");
                let reply = super::super::parse_with(&text, &tools);
                let names: Vec<_> = reply.calls.iter().map(|c| c.name.as_str()).collect();
                if names != [*first, *second] || !reply.errors.is_empty() {
                    failures.push(format!("{first}+{second}: {names:?} {:?}", reply.errors));
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
