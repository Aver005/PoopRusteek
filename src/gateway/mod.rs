//! Шлюз подписок и ключей: аккаунты, маршруты, роли, объяснимый выбор цели.
//!
//! Карта файлов (кто что делает дальше — `.docs/gateway-contracts.md`):
//! - `types`: данные конфига и их проверка;
//! - `backend`: трейт адаптера аккаунта и его возможности;
//! - `handle`: фасад `Gateway`, `Decision`, лимиты, учёт;
//! - `history`: перенос истории при смене цели;
//! - `engine`: движок (`Engine`), выбор цели, cooldown, правки, заглушка секретов;
//! - `routed`: `RoutedProvider`, fallback до первого ушедшего чанка;
//! - `state`, `ledger`, `clock`: состояние аккаунтов, учёт токенов, часы;
//! - `api_backend`, `deepseek_backend`: адаптеры поверх существующих клиентов;
//! - `cli`: адаптеры Claude Code и OpenCode в режиме модели.
//!
//! Шлюз выключен (`[gateway] enabled = false`), поведение приложения прежнее.

pub mod api_backend;
pub mod backend;
pub mod cli;
pub mod clock;
pub mod deepseek_backend;
pub mod engine;
#[cfg(test)]
mod engine_tests;
pub mod handle;
pub mod history;
pub mod ledger;
pub mod pending;
pub mod routed;
#[cfg(test)]
mod routed_tests;
pub mod state;
#[cfg(test)]
pub(crate) mod testkit;
pub mod types;
#[cfg(test)]
mod wiring_tests;
