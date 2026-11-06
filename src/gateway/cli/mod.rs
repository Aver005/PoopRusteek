//! Адаптеры `Backend` для установленных CLI в режиме модели: Claude Code и OpenCode.
//! Наши инструменты и подтверждения, текст на входе, текст с расходом на выходе.
//!
//! Карта: `discover` (бинарь, версия), `launch` (argv, окружение, папки), `process` (процесс,
//! дерево, сторож), `claude`/`opencode` (флаги и разбор потока), `classify` (образцы отказов),
//! `adapter` (сам `Backend`), `codex` (отказ с причиной).

mod adapter;
mod classify;
mod claude;
mod codex;
mod dialect;
pub mod discover;
mod factory;
mod launch;
mod opencode;
pub mod process;
#[cfg(test)]
mod tests;

pub use factory::CliFactory;
