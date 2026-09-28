//! Бюджет повторов внутри одного хода. Модель, которая не умеет исправиться,
//! не должна крутиться до упора в шаги.

/// Сколько раз подряд модели дают переписать сломанный `<tool_use>`.
pub const MAX_MALFORMED_TOOL_RETRIES: u32 = 2;

/// Сколько раз подряд подталкивают модель после пустого ответа.
pub const MAX_EMPTY_RESPONSE_RETRIES: u32 = 2;

/// Разметка похожа на вызов, но её не взял ни один формат. Одна попытка:
/// второй такой же ответ — пример, а не вызов.
const MAX_UNPARSED_MARKUP_RETRIES: u32 = 1;

/// Счётчики повторов одного хода. Оба цикла (главный и суб-агентский) вели
/// их вручную, каждый со своей границей.
#[derive(Debug, Default)]
pub struct RetryBudget {
    malformed: u32,
    empty: u32,
    residue: u32,
}

impl RetryBudget {
    /// Занять попытку на сломанный вызов инструмента. `Some(номер)` — пока
    /// бюджет есть, `None` — исчерпан.
    pub fn take_malformed(&mut self) -> Option<u32> {
        Self::take(&mut self.malformed, MAX_MALFORMED_TOOL_RETRIES)
    }

    /// То же для пустого ответа.
    pub fn take_empty(&mut self) -> Option<u32> {
        Self::take(&mut self.empty, MAX_EMPTY_RESPONSE_RETRIES)
    }

    /// То же для разметки, которую не взял ни один формат.
    pub fn take_residue(&mut self) -> Option<u32> {
        Self::take(&mut self.residue, MAX_UNPARSED_MARKUP_RETRIES)
    }

    /// Шаг выполнил вызовы — счёт «подряд» начинается заново. Без сброса
    /// бюджет был один на весь ход, и ранние сбои съедали поздние попытки.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    fn take(counter: &mut u32, max: u32) -> Option<u32> {
        if *counter >= max {
            return None;
        }
        *counter += 1;
        Some(*counter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_budget_hands_out_exactly_its_limit() {
        let mut budget = RetryBudget::default();
        assert_eq!(budget.take_malformed(), Some(1));
        assert_eq!(budget.take_malformed(), Some(2));
        assert_eq!(budget.take_malformed(), None);
        assert_eq!(budget.take_malformed(), None);
    }

    #[test]
    fn a_step_that_ran_tools_restores_the_budget() {
        let mut budget = RetryBudget::default();
        while budget.take_malformed().is_some() {}
        budget.reset();
        assert_eq!(budget.take_malformed(), Some(1));
    }

    #[test]
    fn the_two_counters_are_independent() {
        // Сломанные вызовы не должны съедать попытки пустых ответов.
        let mut budget = RetryBudget::default();
        while budget.take_malformed().is_some() {}
        assert_eq!(budget.take_empty(), Some(1));
    }
}
