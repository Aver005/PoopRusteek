//! Один клиент HTTP на все разговоры с MCP-серверами. Транспорт и
//! авторизация настраивали себя порознь и разъехались по четырём пунктам.

use crate::error::AppResult;
use std::time::Duration;

/// Соединению свой короткий срок, иначе медленный сервер съедает бюджет,
/// отведённый на сам запрос.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Бюджет протокольного запроса: вызов инструмента и тело SSE бывают долгими.
pub(super) const PROTOCOL_TIMEOUT: Duration = Duration::from_secs(60);

/// Бюджет запроса авторизации: три мелких обмена JSON, падать лучше быстро.
pub(super) const AUTH_TIMEOUT: Duration = Duration::from_secs(15);

// Единственное намеренное расхождение двух клиентов. Если кто-то сведёт
// бюджеты к одному, сборка скажет, что разница была не случайной.
const _: () = assert!(AUTH_TIMEOUT.as_secs() < PROTOCOL_TIMEOUT.as_secs());

/// `read_timeout` равен общему бюджету: без него ответ, капающий байтами
/// достаточно часто, висит сколько угодно, не упираясь в `timeout`.
pub(super) fn client(request_timeout: Duration) -> AppResult<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .cookie_store(true)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(request_timeout)
        .read_timeout(request_timeout)
        .build()?)
}
