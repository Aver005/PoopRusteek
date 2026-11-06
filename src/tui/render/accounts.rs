//! Экран аккаунтов и маршрутов, мастер, скрытый ввод секрета, ограничения, подтверждение проекта.
//! Рисуется только по готовым строкам из `app::gateway_view`; состояние тут не меняется.

use super::popup::{
    center_popup, draw_popup, fill_panel_space, modal_block, option_row, push_text_box_lines,
    separator_line,
};
use super::util::{fit_col, truncate};
use crate::app::gateway_view::forms::{
    LIMIT_FIELDS, LimitsForm, SecretPrompt, TrustChoice, TrustDialog, TrustLineKind, trust_view,
};
use crate::app::gateway_view::rows::{
    AccountRow, DEFAULT_HARD_PCT, DEFAULT_SOFT_PCT, LimitLine, Mark, RouteRow, Tone, chip_text,
    last_decision, role_name, role_rows, route_rows, summary_text,
};
use crate::app::gateway_view::wizard::{
    AccountWizard, KINDS, Kind, LOGIN_OWN, LOGIN_SHARED, PROTOCOLS, Step,
};
use crate::app::gateway_view::{Focus, GatewayView, Notice};
use crate::config::Config;
use crate::gateway::types::DIRECT_TOKEN_WARNING;
use crate::tui::glyphs;
use crate::tui::theme::Theme;
use crate::tui::wrap::wrap_words;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

/// Столько колонок нужно, чтобы показать окна лимитов справа от имени.
const WIDE_COLS: u16 = 110;
const GAUGE_CELLS: usize = 10;

fn tone_color(tone: Tone, theme: &Theme) -> Color {
    match tone {
        Tone::Ok => theme.success,
        Tone::Warn => theme.warning,
        Tone::Bad => theme.error,
        Tone::Dim => theme.text_dim,
    }
}

fn mark_glyph(mark: Mark) -> &'static str {
    let g = glyphs::get();
    match mark {
        Mark::Ready => g.radio_on,
        Mark::Cooling => g.cooling,
        Mark::Down => g.cross,
        Mark::Attention => g.warn,
        Mark::Off => g.radio_off,
    }
}

/// Шкала лимита: сплошная — показание сервера, штриховая — оценка.
fn gauge(line: &LimitLine, theme: &Theme) -> Vec<Span<'static>> {
    let g = glyphs::get();
    let Some(pct) = line.pct else {
        return vec![Span::styled(
            "no reading",
            Style::default().fg(theme.text_dim),
        )];
    };
    let filled = ((pct / 100.0) * GAUGE_CELLS as f32)
        .round()
        .clamp(0.0, GAUGE_CELLS as f32) as usize;
    let color = if line.stale {
        theme.text_dim
    } else if pct >= DEFAULT_HARD_PCT {
        theme.error
    } else if pct >= DEFAULT_SOFT_PCT {
        theme.warning
    } else {
        theme.success
    };
    let fill = if line.estimate {
        g.gauge_est
    } else {
        g.gauge_full
    };
    vec![
        Span::styled(fill.repeat(filled), Style::default().fg(color)),
        Span::styled(
            g.gauge_empty.repeat(GAUGE_CELLS - filled),
            Style::default().fg(theme.text_dim),
        ),
        Span::styled(
            format!(" {}{pct:>3.0}%", if line.estimate { "~" } else { " " }),
            Style::default().fg(color),
        ),
    ]
}

fn limit_spans(line: &LimitLine, theme: &Theme) -> Vec<Span<'static>> {
    let dim = Style::default().fg(theme.text_dim);
    let mut spans = vec![Span::styled(format!("{:<6}", truncate(&line.name, 5)), dim)];
    spans.extend(gauge(line, theme));
    if let Some(resets) = &line.resets {
        spans.push(Span::styled(format!("  {} {}", "resets", resets), dim));
    }
    let age = if line.stale {
        format!("  {} \u{00B7} stale", line.provenance)
    } else {
        format!("  {}", line.provenance)
    };
    spans.push(Span::styled(age, dim));
    spans
}

/// Строки одного аккаунта. Первая — имя и статус, дальше — окна лимитов, модели, заметка.
fn account_lines(
    index: usize,
    row: &AccountRow,
    selected: bool,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let wide = width >= usize::from(WIDE_COLS);
    let bg = if selected { theme.selection } else { theme.bg };
    let base = Style::default().bg(bg);
    let marker = Span::styled(
        if selected { glyphs::get().sel_bar } else { " " },
        base.fg(theme.accent),
    );
    let name_w = if wide { 16 } else { 14 };
    let status_color = tone_color(row.status.tone, theme);
    let head = vec![
        marker.clone(),
        Span::styled(format!("{:>2} ", index + 1), base.fg(theme.text_dim)),
        Span::styled(
            fit_col(&row.label, name_w),
            base.fg(if row.enabled {
                theme.fg
            } else {
                theme.text_dim
            })
            .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {:<4}", row.kind), base.fg(theme.accent_soft)),
        Span::styled(
            format!("{} ", mark_glyph(row.status.mark)),
            base.fg(status_color),
        ),
        Span::styled(
            truncate(&row.status.text, if wide { 22 } else { 26 }),
            base.fg(status_color),
        ),
    ];
    let limit_rows: Vec<&LimitLine> = if wide {
        row.limits.iter().take(3).collect()
    } else {
        row.limits.iter().take(1).collect()
    };
    // Модели и заметка: в широком виде каждое на своей строке.
    let detail_left = |note_only: bool| -> Vec<Span<'static>> {
        let mut spans = vec![Span::styled("      ", base)];
        if !note_only {
            for (model, favorite) in &row.models {
                if *favorite {
                    spans.push(Span::styled(
                        format!("{} ", glyphs::get().star),
                        base.fg(theme.accent),
                    ));
                }
                spans.push(Span::styled(format!("{model}  "), base.fg(theme.text_soft)));
            }
        }
        if note_only && let Some(note) = &row.note {
            spans.push(Span::styled(note.clone(), base.fg(theme.warning)));
        }
        spans
    };

    let mut lines = Vec::new();
    if wide {
        // Широкий вид: слева имя/модели/заметка, справа окна лимитов.
        let left_w = 6 + 3 + name_w + 5 + 2 + 24;
        let models_row = usize::from(!row.models.is_empty());
        let note_row = usize::from(row.note.is_some());
        let count = limit_rows.len().max(1 + models_row + note_row);
        for at in 0..count {
            let mut spans = if at == 0 {
                head.clone()
            } else if at == 1 && models_row == 1 {
                detail_left(false)
            } else if note_row == 1 && at == 1 + models_row {
                detail_left(true)
            } else {
                vec![Span::styled(" ", base)]
            };
            let used: usize = spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            if used > left_w {
                // Слишком длинные модели срезаем по границе колонки лимитов.
                spans = clip_spans(spans, left_w);
            }
            let used: usize = spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            spans.push(Span::styled(" ".repeat(left_w.saturating_sub(used)), base));
            match limit_rows.get(at) {
                Some(limit) => {
                    for span in limit_spans(limit, theme) {
                        spans.push(Span::styled(span.content, span.style.bg(bg)));
                    }
                }
                None if at == 0 => {
                    spans.push(Span::styled("no limit data", base.fg(theme.text_dim)))
                }
                None => {}
            }
            lines.push(fill_line(spans, width, bg));
        }
    } else {
        lines.push(fill_line(head, width, bg));
        let mut second = vec![Span::styled("     ", base)];
        match limit_rows.first() {
            Some(limit) => {
                for span in limit_spans(limit, theme) {
                    second.push(Span::styled(span.content, span.style.bg(bg)));
                }
            }
            None => {
                let text = row
                    .note
                    .clone()
                    .unwrap_or_else(|| "no limit data".to_string());
                second.push(Span::styled(text, base.fg(theme.text_dim)));
            }
        }
        lines.push(fill_line(second, width, bg));
    }
    lines
}

fn clip_spans(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let w = UnicodeWidthStr::width(span.content.as_ref());
        if used + w <= max {
            used += w;
            out.push(span);
        } else {
            let room = max.saturating_sub(used);
            out.push(Span::styled(truncate(&span.content, room), span.style));
            break;
        }
    }
    out
}

/// Дополнить строку пробелами фона до ширины: выделение заливает всю строку.
fn fill_line(mut spans: Vec<Span<'static>>, width: usize, bg: Color) -> Line<'static> {
    let used: usize = spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    if used < width {
        spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(bg),
        ));
    }
    Line::from(spans)
}

fn rule(width: usize, label: &str, theme: &Theme) -> Line<'static> {
    let dash = "\u{2500}";
    if label.is_empty() {
        return Line::from(Span::styled(
            dash.repeat(width),
            Style::default().fg(theme.border),
        ));
    }
    let head = format!("{dash} {label} ");
    let fill = width.saturating_sub(UnicodeWidthStr::width(head.as_str()));
    Line::from(vec![
        Span::styled(
            head,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(dash.repeat(fill), Style::default().fg(theme.border)),
    ])
}

fn chain_spans(route: &RouteRow, focus_step: Option<usize>, theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, step) in route.steps.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(
                " \u{2192} ",
                Style::default().fg(theme.text_dim),
            ));
        }
        let focused = focus_step == Some(i);
        let name_style = if step.current {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg)
        };
        let mut bg = Style::default();
        if focused {
            bg = bg.bg(theme.selection);
        }
        spans.push(Span::styled(
            format!("{} ", step.number),
            bg.fg(theme.text_dim),
        ));
        spans.push(Span::styled(
            format!("{} ", mark_glyph(step.mark)),
            bg.fg(tone_color(step.tone, theme)),
        ));
        let model = step
            .model
            .as_deref()
            .map(|m| format!("/{m}"))
            .unwrap_or_default();
        spans.push(Span::styled(
            format!("{}{model}", step.account),
            name_style.patch(bg),
        ));
    }
    spans
}

fn routes_block(view: &GatewayView, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let Some(snapshot) = &view.snapshot else {
        return Vec::new();
    };
    let routes = route_rows(snapshot, view.now());
    let focus = view.screen.focus;
    let first = &routes.first();
    let detail = first.map_or(String::new(), |r| r.detail.clone());
    let mut lines = vec![rule(
        width,
        &format!(
            "ROUTES \u{00B7} {}",
            truncate(&detail, width.saturating_sub(12))
        ),
        theme,
    )];
    if routes.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no routes yet: add an account with a and it gets one",
            Style::default().fg(theme.text_dim),
        )));
    }
    for (i, route) in routes.iter().take(3).enumerate() {
        let picked = focus == Focus::Routes && view.screen.route == i;
        let mut spans = vec![
            Span::styled(
                if picked { glyphs::get().sel_bar } else { " " },
                Style::default().fg(theme.accent),
            ),
            Span::styled(
                format!(" {:<9}", truncate(&route.id, 8)),
                Style::default()
                    .fg(if route.main { theme.accent } else { theme.fg })
                    .add_modifier(if route.main {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
        ];
        spans.extend(chain_spans(
            route,
            picked.then_some(view.screen.step),
            theme,
        ));
        lines.push(Line::from(clip_spans(spans, width)));
    }
    // Роли: у каждой свой маршрут или наследование от главной.
    let mut roles = vec![Span::styled(
        " ROLES    ",
        Style::default().fg(theme.text_dim),
    )];
    for (i, (role, route)) in role_rows(snapshot).iter().enumerate() {
        if i > 0 {
            roles.push(Span::styled(
                " \u{00B7} ",
                Style::default().fg(theme.text_dim),
            ));
        }
        let picked = focus == Focus::Roles && view.screen.role == i;
        let style = if picked {
            Style::default()
                .fg(theme.fg)
                .bg(theme.selection)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text_soft)
        };
        roles.push(Span::styled(
            format!("{} \u{2192} {route}", role_name(*role)),
            style,
        ));
    }
    lines.push(Line::from(clip_spans(roles, width)));
    lines
}

fn details_block(
    view: &GatewayView,
    config: &Config,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let rows = view.account_rows();
    let Some(row) = rows.get(view.screen.selected) else {
        return Vec::new();
    };
    let account = config.accounts.iter().find(|a| a.id.as_str() == row.id);
    let dim = Style::default().fg(theme.text_dim);
    let text = Style::default().fg(theme.fg);
    let mut lines = vec![rule(
        width,
        &format!("{} \u{00B7} {}", row.label, row.kind),
        theme,
    )];
    let kv = |label: &str, value: String| {
        Line::from(vec![
            Span::styled(format!("  {label:<10}"), dim),
            Span::styled(truncate(&value, width.saturating_sub(14)), text),
        ])
    };
    lines.push(kv("status", row.status.text.clone()));
    if let Some(account) = account {
        if let Some(url) = &account.base_url {
            lines.push(kv("address", url.clone()));
        }
        if let Some(home) = &account.home {
            lines.push(kv("login", format!("own folder {}", home.display())));
        } else if account.provider.is_cli() {
            lines.push(kv(
                "login",
                "shared with the CLI on this machine".to_string(),
            ));
        }
        let l = &account.limits;
        let mut set = Vec::new();
        if let Some(v) = l.soft_pct {
            set.push(format!("soft {v}%"));
        }
        if let Some(v) = l.hard_pct {
            set.push(format!("hard {v}%"));
        }
        if let Some(v) = l.requests_per_minute {
            set.push(format!("{v}/min"));
        }
        if let Some(v) = l.max_concurrency {
            set.push(format!("{v} at once"));
        }
        if let Some(v) = l.budget_usd_month {
            set.push(format!("${v}/month"));
        }
        lines.push(kv(
            "limits set",
            if set.is_empty() {
                "none (e to edit)".into()
            } else {
                set.join(" \u{00B7} ")
            },
        ));
    }
    for limit in &row.limits {
        let mut spans = vec![Span::styled(format!("  {:<10}", "window"), dim)];
        spans.extend(limit_spans(limit, theme));
        lines.push(Line::from(spans));
    }
    let models: Vec<String> = row
        .models
        .iter()
        .map(|(m, fav)| {
            if *fav {
                format!("{} {m}", glyphs::get().star)
            } else {
                m.clone()
            }
        })
        .collect();
    if !models.is_empty() {
        lines.push(kv("models", models.join("   ")));
    }
    lines
}

fn decision_block(
    view: &GatewayView,
    width: usize,
    theme: &Theme,
    history: bool,
) -> Vec<Line<'static>> {
    let Some(snapshot) = &view.snapshot else {
        return Vec::new();
    };
    let now = view.now();
    let dim = Style::default().fg(theme.text_dim);
    if history {
        let mut lines = vec![rule(width, "RECENT DECISIONS", theme)];
        for decision in snapshot.recent.iter().rev().take(6) {
            let d = crate::app::gateway_view::rows::decision_view(snapshot, decision, now);
            let color = if d.ok { theme.success } else { theme.error };
            let mark = if d.ok {
                glyphs::get().check
            } else {
                glyphs::get().cross
            };
            lines.push(Line::from(vec![
                Span::styled(format!(" {mark} "), Style::default().fg(color)),
                Span::styled(
                    truncate(&format!("{}  {}", d.head, d.raw), width.saturating_sub(4)),
                    Style::default().fg(theme.text_soft),
                ),
            ]));
        }
        if snapshot.recent.is_empty() {
            lines.push(Line::from(Span::styled("  nothing decided yet", dim)));
        }
        return lines;
    }
    let Some(d) = last_decision(snapshot, now) else {
        return vec![
            rule(width, "LAST DECISION", theme),
            Line::from(Span::styled("  no turn has used the route yet", dim)),
        ];
    };
    let g = glyphs::get();
    let color = if d.ok { theme.success } else { theme.error };
    let mut lines = vec![rule(
        width,
        &format!("LAST DECISION \u{00B7} {}", d.head),
        theme,
    )];
    lines.push(Line::from(vec![
        Span::styled(
            format!("  {} ", if d.ok { g.check } else { g.cross }),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            truncate(&d.headline, width.saturating_sub(6)),
            Style::default().fg(theme.fg),
        ),
    ]));
    for skipped in d.skipped.iter().take(2) {
        lines.push(Line::from(vec![
            Span::styled("    skipped  ", dim),
            Span::styled(truncate(skipped, width.saturating_sub(14)), dim),
        ]));
    }
    lines.push(Line::from(vec![
        Span::styled("    raw      ", dim),
        Span::styled(truncate(&d.raw, width.saturating_sub(14)), dim),
    ]));
    lines
}

fn problem_line(view: &GatewayView, width: usize, theme: &Theme) -> Option<Line<'static>> {
    use crate::gateway::types::Severity;
    let snapshot = view.snapshot.as_ref()?;
    let (problem, color) = match snapshot
        .problems
        .iter()
        .find(|p| p.severity == Severity::Error)
    {
        Some(error) => (error, theme.error),
        // Пока шлюз собирается, это единственное объяснение пустого списка.
        None => (
            snapshot
                .problems
                .iter()
                .find(|p| p.path == "gateway" && p.severity == Severity::Warning)?,
            theme.warning,
        ),
    };
    Some(Line::from(Span::styled(
        truncate(
            &format!(
                " {} {}: {}",
                glyphs::get().warn,
                problem.path,
                problem.message
            ),
            width,
        ),
        Style::default().fg(color),
    )))
}

/// Подсказки клавиш; что не влезло по ширине, отбрасывается с конца.
fn hint_line(view: &GatewayView, width: usize, enabled: bool, theme: &Theme) -> Line<'static> {
    let hints: &[&str] = match view.screen.focus {
        _ if !enabled => &[
            "\u{2191}\u{2193} select",
            "Enter make active",
            "a add",
            "g turn the gateway on",
            "Esc back",
        ],
        Focus::Accounts => &[
            "\u{2191}\u{2193} select",
            "Enter details",
            "K/J rank",
            "Space on/off",
            "a add",
            "e limits",
            "r route",
            "p pin chat",
            "d remove",
            "h history",
            "Tab routes",
            "Esc back",
        ],
        Focus::Routes => &[
            "\u{2191}\u{2193} step",
            "Left/Right route",
            "K/J rank",
            "x remove step",
            "Tab roles",
            "Esc back",
        ],
        Focus::Roles => &[
            "\u{2191}\u{2193} role",
            "Left/Right route",
            "Tab accounts",
            "Esc back",
        ],
    };
    // Esc всегда остаётся в конце: место под него откладывается заранее, лишнее выпадает из середины.
    let Some((last, middle)) = hints.split_last() else {
        return Line::default();
    };
    let tail = format!(" \u{00B7} {last}");
    let budget = width.saturating_sub(UnicodeWidthStr::width(tail.as_str()) + 1);
    let mut text = String::new();
    for (i, hint) in middle.iter().enumerate() {
        let next = if i == 0 {
            format!(" {hint}")
        } else {
            format!(" \u{00B7} {hint}")
        };
        if UnicodeWidthStr::width(text.as_str()) + UnicodeWidthStr::width(next.as_str()) > budget {
            continue;
        }
        text.push_str(&next);
    }
    text.push_str(&tail);
    Line::from(Span::styled(text, Style::default().fg(theme.text_dim)))
}

fn notice_line(notice: &Notice, width: usize, theme: &Theme) -> Line<'static> {
    let (glyph, color) = if notice.error {
        (glyphs::get().warn, theme.error)
    } else {
        (glyphs::get().check, theme.success)
    };
    Line::from(Span::styled(
        truncate(&format!(" {glyph} {}", notice.text), width),
        Style::default().fg(color),
    ))
}

fn off_screen(frame: &mut Frame, area: Rect, config: &Config, view: &GatewayView, theme: &Theme) {
    let dim = Style::default().fg(theme.text_dim);
    let soft = Style::default().fg(theme.text_soft);
    let mut lines = vec![
        Line::from(Span::styled(
            "  The gateway is off: your providers work the old way, one at a time.",
            Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "  Press g to turn it on: they become accounts in one route. Nothing is deleted.",
            soft,
        )),
        Line::from(""),
    ];
    let active = config
        .active_provider_entry()
        .map(|entry| entry.name.as_str());
    let width = usize::from(area.width);
    let mut rows: Vec<(String, String, bool)> = vec![(
        "DeepSeek (built-in)".to_string(),
        config.provider.model.clone(),
        active.is_none(),
    )];
    rows.extend(config.providers.iter().map(|entry| {
        (
            entry.name.clone(),
            format!("{} \u{00B7} {}", entry.model, entry.base_url),
            active == Some(entry.name.as_str()),
        )
    }));
    for (i, (name, detail, is_active)) in rows.iter().enumerate() {
        let selected = i == view.screen.selected;
        let mark = if *is_active {
            glyphs::get().radio_on
        } else {
            glyphs::get().radio_off
        };
        let style = if selected {
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)
        } else {
            soft
        };
        let text = format!(
            "  {} {mark} {name}{}  {detail}",
            if selected { glyphs::get().pointer } else { " " },
            if *is_active { "  (active)" } else { "" },
        );
        lines.push(Line::from(Span::styled(truncate(&text, width), style)));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Enter makes it active; a adds a provider (API key or local server).",
        dim,
    )));
    frame.render_widget(Paragraph::new(lines), area);
}

pub(super) fn render_accounts_view(
    frame: &mut Frame,
    area: Rect,
    view: &GatewayView,
    enabled: bool,
    config: &Config,
    theme: &Theme,
) {
    let width = usize::from(area.width);
    let rows = view.account_rows();
    let summary = if enabled {
        if rows.is_empty() {
            "gateway ON".to_string()
        } else {
            format!("gateway ON \u{00B7} {}", summary_text(&rows))
        }
    } else {
        "gateway OFF".to_string()
    };
    let title = " Accounts & routes";
    let gap = width.saturating_sub(
        UnicodeWidthStr::width(title) + UnicodeWidthStr::width(summary.as_str()) + 1,
    );
    let header = Line::from(vec![
        Span::styled(
            title,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(gap)),
        Span::styled(summary, Style::default().fg(theme.text_dim)),
    ]);
    let bottom = area.bottom();
    frame.render_widget(
        Paragraph::new(header),
        Rect::new(area.x, area.y, area.width, 1),
    );
    frame.render_widget(
        Paragraph::new(rule(width, "", theme)),
        Rect::new(area.x, area.y + 1, area.width, 1),
    );
    // Низ: подсказки и черта над ними, заметка или вопрос над чертой.
    let question = if view.screen.confirm_enable && !enabled {
        Some(
            "Turn the gateway on? Your providers become accounts in one route; nothing is deleted. y/n"
                .to_string(),
        )
    } else {
        view.screen.confirm_remove.as_ref().map(|c| c.text.clone())
    };
    let note: Vec<Line<'static>> = match (&question, &view.screen.notice) {
        (Some(text), _) => wrap_words(text, width.saturating_sub(2))
            .into_iter()
            .take(3)
            .map(|row| {
                Line::from(Span::styled(
                    format!(" {row}"),
                    Style::default()
                        .fg(theme.warning)
                        .add_modifier(Modifier::BOLD),
                ))
            })
            .collect(),
        (None, Some(notice)) => vec![notice_line(notice, width, theme)],
        (None, None) => Vec::new(),
    };
    let notice_rows = note.len() as u16;
    frame.render_widget(
        Paragraph::new(hint_line(view, width, enabled, theme)),
        Rect::new(area.x, bottom - 1, area.width, 1),
    );
    frame.render_widget(
        Paragraph::new(rule(width, "", theme)),
        Rect::new(area.x, bottom - 2, area.width, 1),
    );
    if notice_rows > 0 {
        frame.render_widget(
            Paragraph::new(note),
            Rect::new(
                area.x,
                bottom.saturating_sub(2 + notice_rows),
                area.width,
                notice_rows,
            ),
        );
    }
    let top = area.y + 2;
    let body_h = bottom.saturating_sub(top + 2 + notice_rows);
    let body = Rect::new(area.x, top, area.width, body_h);
    if !enabled {
        off_screen(frame, body, config, view, theme);
        return;
    }
    if rows.is_empty() {
        let starting = view.snapshot.as_ref().is_some_and(|s| {
            s.problems
                .iter()
                .any(|p| p.path == "gateway" && p.message.contains("starting"))
        });
        let text = if starting {
            "  The gateway is starting: the accounts are being connected, give it a moment."
        } else {
            "  No accounts yet. a add an API key or a subscription (Claude Code, OpenCode)."
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                text,
                Style::default().fg(theme.text_dim),
            ))),
            body,
        );
        return;
    }

    // Нижние блоки: маршруты (или подробности) и решение. Список забирает остальное.
    let lower_a = if view.screen.detail {
        details_block(view, config, width, theme)
    } else {
        routes_block(view, width, theme)
    };
    let lower_b = decision_block(view, width, theme, view.screen.history);
    let problem = problem_line(view, width, theme);
    let want_lower = lower_a.len() + lower_b.len() + usize::from(problem.is_some());
    let min_list = 4usize;
    let room = usize::from(body_h)
        .saturating_sub(want_lower)
        .max(min_list.min(usize::from(body_h)));

    // Список с окном прокрутки по выбранному аккаунту.
    let mut list: Vec<Line<'static>> = Vec::new();
    let mut starts = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        starts.push(list.len());
        list.extend(account_lines(
            i,
            row,
            i == view.screen.selected,
            width,
            theme,
        ));
    }
    // Нижние блоки идут сразу за списком; пустота остаётся внизу, а не посередине.
    let list_h = list.len().min(room);
    let mut remaining = usize::from(body_h).saturating_sub(list_h);
    let selected_start = starts.get(view.screen.selected).copied().unwrap_or(0);
    let offset = (selected_start + 2).saturating_sub(list_h.max(1));
    let shown: Vec<Line<'static>> = list.into_iter().skip(offset).take(list_h).collect();
    frame.render_widget(
        Paragraph::new(shown).style(Style::default().bg(theme.bg)),
        Rect::new(body.x, body.y, body.width, list_h as u16),
    );

    let mut y = body.y + list_h as u16;
    let mut lower: Vec<Line<'static>> = Vec::new();
    lower.extend(problem);
    lower.extend(lower_a);
    lower.extend(lower_b);
    // Что не влезло, отбрасывается снизу (подробности решения), а не сжимает список.
    lower.truncate(remaining);
    remaining = lower.len();
    frame.render_widget(
        Paragraph::new(lower),
        Rect::new(body.x, y, body.width, remaining as u16),
    );
    y += remaining as u16;
    let _ = y;
}

// ── Модалки ─────────────────────────────────────────────────────────────

fn field_hint(text: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {text}"),
        Style::default().fg(theme.text_dim),
    ))
}

fn wrapped(lines: &mut Vec<Line<'static>>, text: &str, width: usize, style: Style) {
    for row in wrap_words(text, width) {
        lines.push(Line::from(Span::styled(format!("  {row}"), style)));
    }
}

pub(super) fn render_account_wizard(
    frame: &mut Frame,
    area: Rect,
    wizard: &AccountWizard,
    theme: &Theme,
) {
    let popup_w = area.width.clamp(52, 86);
    let want = match wizard.step {
        Step::Consent => 17,
        Step::Login | Step::Confirm => 15,
        Step::BaseUrl => 13,
        _ => 12,
    };
    let popup_h = want.min(area.height.saturating_sub(2));
    let popup_area = center_popup(area, popup_w, popup_h);
    let block = modal_block(" Add account ", theme.accent, theme);
    let inner = block.inner(popup_area);
    let inner_w = usize::from(inner.width);
    let text_w = inner_w.saturating_sub(6);
    let max_rows = usize::from(inner.height).saturating_sub(8).max(1);
    let (at, total) = wizard.position();
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(vec![
            Span::styled(
                format!("  Step {at}/{total} \u{2014} "),
                Style::default().fg(theme.text_dim),
            ),
            Span::styled(
                wizard.step.title(),
                Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
    ];
    match wizard.step {
        Step::Kind => {
            for (i, kind) in KINDS.iter().enumerate() {
                lines.push(option_row(kind.label(), i == wizard.kind, theme));
            }
        }
        Step::Label => {
            push_text_box_lines(
                &mut lines,
                &wizard.label.buffer,
                wizard.label.cursor,
                theme,
                max_rows,
            );
            lines.push(field_hint("e.g. claude-max, work, local lm", theme));
        }
        Step::Protocol => {
            for (i, (_, label)) in PROTOCOLS.iter().enumerate() {
                lines.push(option_row(*label, i == wizard.protocol, theme));
            }
        }
        Step::BaseUrl => {
            let protocol = PROTOCOLS[wizard.protocol.min(PROTOCOLS.len() - 1)].0;
            let example = protocol
                .protocol()
                .map(crate::app::providers::base_url_example)
                .unwrap_or("");
            wrapped(
                &mut lines,
                example,
                text_w,
                Style::default().fg(theme.text_dim),
            );
            push_text_box_lines(
                &mut lines,
                &wizard.base_url.buffer,
                wizard.base_url.cursor,
                theme,
                max_rows,
            );
        }
        Step::Model => {
            push_text_box_lines(
                &mut lines,
                &wizard.model.buffer,
                wizard.model.cursor,
                theme,
                max_rows,
            );
            lines.push(field_hint("blank = choose later in /models", theme));
        }
        Step::Login => {
            for (i, label) in [LOGIN_OWN, LOGIN_SHARED].iter().enumerate() {
                let rows = wrap_words(label, text_w);
                for (n, row) in rows.into_iter().enumerate() {
                    if n == 0 {
                        lines.push(option_row(row, i == wizard.login, theme));
                    } else {
                        lines.push(Line::from(Span::styled(
                            format!("      {row}"),
                            Style::default().fg(theme.text_dim),
                        )));
                    }
                }
            }
        }
        Step::Consent => {
            wrapped(
                &mut lines,
                &format!("{} {DIRECT_TOKEN_WARNING}.", glyphs::get().warn),
                text_w,
                Style::default().fg(theme.warning),
            );
            lines.push(Line::from(""));
            for (i, label) in [
                "Cancel: use the official CLI instead",
                "I understand the risk, continue",
            ]
            .iter()
            .enumerate()
            {
                lines.push(option_row(*label, i == wizard.consent_choice, theme));
            }
        }
        Step::Secret => {
            let masked = wizard.secret.mask();
            push_text_box_lines(&mut lines, &masked, wizard.secret.len(), theme, max_rows);
            lines.push(field_hint(
                "Stored in the system keyring, never in the config file.",
                theme,
            ));
        }
        Step::Confirm => confirm_lines(&mut lines, wizard, text_w, theme),
    }
    if let Some(error) = &wizard.error {
        lines.push(Line::from(""));
        wrapped(
            &mut lines,
            &format!("{} {error}", glyphs::get().warn),
            text_w,
            Style::default().fg(theme.error),
        );
    }
    let hint = match wizard.step {
        Step::Kind | Step::Protocol | Step::Login | Step::Consent => {
            " \u{2191}\u{2193} choose    Enter next    Esc back "
        }
        Step::Confirm => " Enter add account    Esc back ",
        _ => " Enter next    Esc back ",
    };
    fill_panel_space(&mut lines, usize::from(inner.height).saturating_sub(2));
    lines.push(separator_line(inner_w, theme));
    lines.push(Line::from(Span::styled(
        hint,
        Style::default().fg(theme.text_dim),
    )));
    draw_popup(frame, popup_area, inner, block, lines, theme);
}

fn confirm_lines(
    lines: &mut Vec<Line<'static>>,
    wizard: &AccountWizard,
    width: usize,
    theme: &Theme,
) {
    let row = |label: &str, value: String| {
        Line::from(vec![
            Span::styled(format!("  {label:<9}"), Style::default().fg(theme.text_dim)),
            Span::styled(
                truncate(&value, width.saturating_sub(8)),
                Style::default().fg(theme.fg),
            ),
        ])
    };
    let kind = wizard.kind();
    lines.push(row("Name", wizard.label.buffer.trim().to_string()));
    lines.push(row("Type", kind.label().to_string()));
    match kind {
        Kind::Api | Kind::DirectToken => {
            lines.push(row("Address", wizard.base_url.buffer.trim().to_string()));
            lines.push(row(
                "Secret",
                if wizard.secret.is_empty() {
                    "(none)".into()
                } else {
                    "set, goes to the keyring".into()
                },
            ));
        }
        Kind::ClaudeCode | Kind::OpenCode => {
            lines.push(row(
                "Login",
                if wizard.login == 0 {
                    "own folder".into()
                } else {
                    "shared with the CLI".into()
                },
            ));
        }
    }
    let model = wizard.model.buffer.trim();
    if !model.is_empty() || kind != Kind::ClaudeCode {
        lines.push(row(
            "Model",
            if model.is_empty() {
                "(choose later)".into()
            } else {
                model.to_string()
            },
        ));
    }
    if matches!(kind, Kind::ClaudeCode | Kind::OpenCode) {
        lines.push(Line::from(""));
        let note = if wizard.login == 0 {
            "After adding, log in once; the command is shown then."
        } else {
            "Shared login: signing out there, or a parallel session, may log it out."
        };
        wrapped(
            lines,
            note,
            width,
            Style::default().fg(if wizard.login == 0 {
                theme.text_dim
            } else {
                theme.warning
            }),
        );
    }
}

pub(super) fn render_secret_prompt(
    frame: &mut Frame,
    area: Rect,
    prompt: &SecretPrompt,
    theme: &Theme,
) {
    let popup_w = area.width.clamp(40, 64);
    let popup_h = 10u16.min(area.height.saturating_sub(2));
    let popup_area = center_popup(area, popup_w, popup_h);
    let block = modal_block(prompt.title(), theme.accent, theme);
    let inner = block.inner(popup_area);
    let inner_w = usize::from(inner.width);
    let mut lines = vec![
        Line::from(Span::styled(
            "  Type or paste the value. It stays hidden.",
            Style::default().fg(theme.text_dim),
        )),
        Line::from(""),
    ];
    let masked = prompt.value.mask();
    push_text_box_lines(&mut lines, &masked, prompt.value.len(), theme, 1);
    if let Some(error) = &prompt.error {
        lines.push(Line::from(Span::styled(
            format!("  {} {error}", glyphs::get().warn),
            Style::default().fg(theme.error),
        )));
    }
    fill_panel_space(&mut lines, usize::from(inner.height).saturating_sub(2));
    lines.push(separator_line(inner_w, theme));
    lines.push(Line::from(Span::styled(
        " Enter store    Ctrl+U clear    Esc cancel ",
        Style::default().fg(theme.text_dim),
    )));
    draw_popup(frame, popup_area, inner, block, lines, theme);
}

pub(super) fn render_limits(frame: &mut Frame, area: Rect, form: &LimitsForm, theme: &Theme) {
    let popup_w = area.width.clamp(52, 80);
    let popup_h = 16u16.min(area.height.saturating_sub(2));
    let popup_area = center_popup(area, popup_w, popup_h);
    let block = modal_block(format!(" Limits: {} ", form.name), theme.accent, theme);
    let inner = block.inner(popup_area);
    let inner_w = usize::from(inner.width);
    let mut lines = vec![
        field_hint("Blank = no limit. Favorites show first.", theme),
        Line::from(""),
    ];
    for (i, field) in LIMIT_FIELDS.iter().enumerate() {
        let focused = i == form.focus;
        let bg = if focused {
            theme.selection
        } else {
            theme.panel
        };
        let value = &form.fields[i].buffer;
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {} ", if focused { glyphs::get().sel_bar } else { " " }),
                Style::default().fg(theme.accent).bg(bg),
            ),
            Span::styled(
                fit_col(field.label(), 36),
                Style::default().fg(theme.fg).bg(bg),
            ),
            Span::styled(
                format!(
                    " {}{}",
                    value,
                    if focused { glyphs::get().caret } else { "" }
                ),
                Style::default().fg(theme.accent_soft).bg(bg),
            ),
        ]));
    }
    if let Some(error) = &form.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(
                "  {} {}",
                glyphs::get().warn,
                truncate(error, inner_w.saturating_sub(6))
            ),
            Style::default().fg(theme.error),
        )));
    }
    fill_panel_space(&mut lines, usize::from(inner.height).saturating_sub(2));
    lines.push(separator_line(inner_w, theme));
    lines.push(Line::from(Span::styled(
        " \u{2191}\u{2193} field    Enter save    Esc cancel ",
        Style::default().fg(theme.text_dim),
    )));
    draw_popup(frame, popup_area, inner, block, lines, theme);
}

pub(super) fn render_trust(frame: &mut Frame, area: Rect, dialog: &TrustDialog, theme: &Theme) {
    let view = trust_view(area.width, area.height);
    let doc = dialog.lines(view.text_w);
    let room = doc.len().min(view.max_room);
    let popup_h = (room + 6).min(usize::from(area.height.saturating_sub(2))) as u16;
    let popup_area = center_popup(area, view.popup_w, popup_h);
    let block = modal_block(" Trust this project config? ", theme.warning, theme);
    let inner = block.inner(popup_area);
    let inner_w = usize::from(inner.width);
    let offset = dialog.offset.min(doc.len().saturating_sub(room));
    let mut lines: Vec<Line<'static>> = doc[offset..offset + room]
        .iter()
        .map(|line| {
            let style = match line.kind {
                TrustLineKind::Summary => Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
                TrustLineKind::Heading => {
                    Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)
                }
                TrustLineKind::Item => Style::default().fg(theme.warning),
                TrustLineKind::Plain => Style::default().fg(theme.text_soft),
                TrustLineKind::Blank => Style::default(),
            };
            Line::from(Span::styled(line.text.clone(), style))
        })
        .collect();
    // Строка над кнопками: почему «Trust» закрыт или сколько строк ещё не показано.
    let below = doc.len().saturating_sub(offset + room);
    let status = match (&dialog.notice, below) {
        (Some(notice), _) => Some((notice.clone(), theme.error)),
        (None, 0) => None,
        (None, n) => Some((format!("\u{2193} {n} more line(s) below"), theme.text_dim)),
    };
    lines.push(match status {
        Some((text, color)) => Line::from(Span::styled(
            truncate(&format!("  {text}"), inner_w),
            Style::default().fg(color),
        )),
        None => Line::from(""),
    });
    lines.push(separator_line(inner_w, theme));
    let readable = dialog.fully_read(&view);
    let button = |label: &str, active: bool, color: Color| {
        let style = if active {
            Style::default()
                .fg(theme.bg)
                .bg(color)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text_dim)
        };
        Span::styled(format!(" {label} "), style)
    };
    lines.push(Line::from(vec![
        Span::raw("  "),
        button(
            "Deny (keep blocked)",
            dialog.choice == TrustChoice::Deny,
            theme.success,
        ),
        Span::raw("   "),
        button(
            if readable {
                "Trust this file"
            } else {
                "Trust this file (read it all first)"
            },
            dialog.choice == TrustChoice::Trust,
            theme.error,
        ),
    ]));
    lines.push(Line::from(Span::styled(
        truncate(
            " Up/Down PgUp/PgDn scroll    Tab choose    Enter confirm    Esc deny ",
            inner_w,
        ),
        Style::default().fg(theme.text_dim),
    )));
    draw_popup(frame, popup_area, inner, block, lines, theme);
}

/// Фишки аккаунтов для главной: `● claude-max 52% · ◔ opencode-go 2m 10s`.
pub(super) fn account_chips(
    view: &GatewayView,
    width: usize,
    theme: &Theme,
) -> Option<Line<'static>> {
    let rows = view.account_rows();
    if rows.is_empty() {
        return None;
    }
    let mut spans = vec![Span::styled(
        " accounts  ",
        Style::default().fg(theme.text_dim),
    )];
    let mut used = UnicodeWidthStr::width(" accounts  ");
    let mut hidden = 0usize;
    for (i, row) in rows.iter().enumerate() {
        let chip = chip_text(row);
        let cost = UnicodeWidthStr::width(chip.as_str()) + 4 + if i > 0 { 3 } else { 0 };
        if used + cost + 6 > width && i > 0 {
            hidden = rows.len() - i;
            break;
        }
        if i > 0 {
            spans.push(Span::styled(
                " \u{00B7} ",
                Style::default().fg(theme.text_dim),
            ));
            used += 3;
        }
        spans.push(Span::styled(
            format!("{} ", mark_glyph(row.status.mark)),
            Style::default().fg(tone_color(row.status.tone, theme)),
        ));
        spans.push(Span::styled(
            chip.clone(),
            Style::default().fg(theme.text_soft),
        ));
        used += UnicodeWidthStr::width(chip.as_str()) + 2;
    }
    if hidden > 0 {
        spans.push(Span::styled(
            format!("  +{hidden}"),
            Style::default().fg(theme.text_dim),
        ));
    }
    Some(Line::from(spans))
}

/// Кто сейчас отвечает: короткий индикатор для подвала. `None`, если аккаунтов меньше двух.
pub(super) fn footer_account(view: &GatewayView) -> Option<(Mark, Tone, String, usize)> {
    let snapshot = view.snapshot.as_ref()?;
    if snapshot.accounts.len() < 2 {
        return None;
    }
    let rows = view.account_rows();
    let current = crate::app::gateway_view::rows::current_account(snapshot)
        .and_then(|id| rows.iter().find(|r| r.id == id))
        .or_else(|| rows.iter().find(|r| r.status.mark == Mark::Ready))
        .or(rows.first())?;
    Some((
        current.status.mark,
        current.status.tone,
        current.label.clone(),
        rows.len() - 1,
    ))
}

pub(super) fn footer_glyph(mark: Mark) -> &'static str {
    mark_glyph(mark)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TrustRequest;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn frame_text(cols: u16, rows: u16, draw: impl FnOnce(&mut Frame)) -> String {
        let mut terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap();
        terminal.draw(draw).unwrap();
        let buffer = terminal.backend().buffer();
        (0..rows)
            .map(|y| {
                (0..cols)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn request(accounts: usize, evil_last: bool) -> TrustRequest {
        let mut held_back = Vec::new();
        let mut hosts = Vec::new();
        for n in 0..accounts {
            let host = if evil_last && n + 1 == accounts {
                "attacker.example".to_string()
            } else {
                format!("fine{n}.example")
            };
            held_back.push(format!(
                "account 'filler-{n}' sends requests to https://{host}/v1"
            ));
            hosts.push(host);
        }
        let mut request = TrustRequest {
            config_path: "repo/.pooprusteek/config.toml".into(),
            key: "k".into(),
            hash: "h".into(),
            held_back,
            summary: Default::default(),
        };
        request.summary.hosts = hosts;
        request.summary.accounts = accounts;
        request
    }

    fn show(dialog: &TrustDialog) -> String {
        frame_text(80, 24, |frame| {
            let area = frame.area();
            render_trust(frame, area, dialog, &Theme::default());
        })
    }

    /// Опасный хост последним из тридцати виден уже на первом экране 80×24.
    #[test]
    fn a_foreign_host_behind_thirty_accounts_is_on_the_first_screen() {
        let dialog = TrustDialog::new(request(30, true));
        let text = show(&dialog);
        assert!(text.contains("attacker.example"), "{text}");
        assert!(text.contains("30 accounts"), "{text}");
        assert!(
            text.contains("more line"),
            "the hidden tail is announced: {text}"
        );
        assert!(text.contains("read it all first"), "{text}");
    }

    /// Обратная сторона: обычный файл из двух записей читается целиком и доверие открыто.
    #[test]
    fn an_ordinary_two_account_file_shows_everything_and_trust_is_open() {
        let dialog = TrustDialog::new(request(2, false));
        let text = show(&dialog);
        assert!(
            text.contains("fine0.example") && text.contains("fine1.example"),
            "{text}"
        );
        assert!(text.contains("2 accounts"), "{text}");
        assert!(!text.contains("more line"), "{text}");
        assert!(text.contains("Trust this file"), "{text}");
        assert!(!text.contains("read it all first"), "{text}");
    }

    /// Прокрутка до конца показывает последние записи.
    #[test]
    fn scrolling_reveals_the_end_of_the_list() {
        let mut dialog = TrustDialog::new(request(30, true));
        let view = crate::app::gateway_view::forms::trust_view(80, 24);
        dialog.reveal(&view);
        dialog.scroll_to(true, &view);
        let text = show(&dialog);
        assert!(text.contains("filler-29"), "{text}");
    }
}
