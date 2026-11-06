//! Правки конфига шлюза из клавиш экрана: чистые функции «снимок → `ConfigEdit`».
//! Записывает их только `Gateway::apply_edit`; здесь ничего не меняется.

use crate::gateway::handle::{ConfigEdit, GatewaySnapshot};
use crate::gateway::types::{AccountId, Role, Route, RouteId, Step};

/// Подвинуть аккаунт в списке на `delta` строк. Порядок списка — приоритет новых маршрутов.
pub fn move_account(
    snapshot: &GatewaySnapshot,
    at: usize,
    delta: isize,
) -> Option<(ConfigEdit, usize)> {
    let view = snapshot.accounts.get(at)?;
    let to = at.checked_add_signed(delta)?;
    (to < snapshot.accounts.len() && to != at).then(|| {
        (
            ConfigEdit::MoveAccount {
                id: view.id.clone(),
                to_index: to,
            },
            to,
        )
    })
}

pub fn toggle_account(snapshot: &GatewaySnapshot, at: usize) -> Option<ConfigEdit> {
    let view = snapshot.accounts.get(at)?;
    Some(ConfigEdit::SetEnabled {
        id: view.id.clone(),
        enabled: !view.enabled,
    })
}

/// Подвинуть шаг маршрута: так меняется ранжирование аккаунтов в цепочке.
pub fn move_step(route: &Route, at: usize, delta: isize) -> Option<(ConfigEdit, usize)> {
    let to = at.checked_add_signed(delta)?;
    if at >= route.steps.len() || to >= route.steps.len() || to == at {
        return None;
    }
    let mut next = route.clone();
    next.steps.swap(at, to);
    Some((ConfigEdit::UpsertRoute(Box::new(next)), to))
}

pub fn remove_step(route: &Route, at: usize) -> Option<ConfigEdit> {
    if at >= route.steps.len() || route.steps.len() == 1 {
        return None;
    }
    let mut next = route.clone();
    next.steps.remove(at);
    Some(ConfigEdit::UpsertRoute(Box::new(next)))
}

/// Есть ли аккаунт в маршруте.
pub fn in_route(route: &Route, account: &AccountId) -> bool {
    route.steps.iter().any(|s| &s.account == account)
}

/// Добавить аккаунт последним шагом или убрать его из маршрута.
pub fn toggle_membership(route: &Route, account: &AccountId) -> Option<ConfigEdit> {
    let mut next = route.clone();
    if in_route(route, account) {
        if route.steps.len() == 1 {
            return None;
        }
        next.steps.retain(|s| &s.account != account);
    } else {
        next.steps.push(Step {
            account: account.clone(),
            model: None,
        });
    }
    Some(ConfigEdit::UpsertRoute(Box::new(next)))
}

/// Маршрут главной роли, а если роли нет — первый.
pub fn main_route(snapshot: &GatewaySnapshot) -> Option<&Route> {
    snapshot
        .roles
        .route_for(Role::Main)
        .and_then(|id| snapshot.routes.iter().find(|r| &r.id == id))
        .or_else(|| snapshot.routes.first())
}

/// Роль на следующий (или предыдущий) маршрут; у не-главных ролей перед первым стоит «наследует main».
pub fn cycle_role(snapshot: &GatewaySnapshot, role: Role, forward: bool) -> Option<ConfigEdit> {
    let routes: Vec<&RouteId> = snapshot.routes.iter().map(|r| &r.id).collect();
    if routes.is_empty() {
        return None;
    }
    // Позиция в кольце: 0 — «нет записи», далее маршруты.
    let slots = routes.len() + usize::from(role != Role::Main);
    let first_route = usize::from(role != Role::Main);
    let current = snapshot
        .roles
        .0
        .get(&role)
        .and_then(|id| routes.iter().position(|r| *r == id))
        .map_or(0, |i| i + first_route);
    let next = if forward {
        (current + 1) % slots
    } else {
        (current + slots - 1) % slots
    };
    let route = next.checked_sub(first_route).map(|i| routes[i].clone());
    Some(ConfigEdit::SetRole { role, route })
}

/// Шаги маршрута в порядке списка аккаунтов; `None`, если порядок уже такой.
/// Так «ранжирование» кнопками K/J в списке переносится в главный маршрут.
pub fn resort_route(snapshot: &GatewaySnapshot, route: &Route) -> Option<ConfigEdit> {
    let position = |step: &Step| {
        snapshot
            .accounts
            .iter()
            .position(|a| a.id == step.account)
            .unwrap_or(usize::MAX)
    };
    let mut next = route.clone();
    next.steps.sort_by_key(position);
    (next.steps != route.steps).then(|| ConfigEdit::UpsertRoute(Box::new(next)))
}

/// Маршрут `main` для первого запуска: все годные аккаунты по порядку списка.
pub fn starter_route(snapshot_accounts: &[crate::gateway::types::Account]) -> Option<Route> {
    use crate::config::ToolProtocol;
    let usable: Vec<&crate::gateway::types::Account> = snapshot_accounts
        .iter()
        .filter(|a| a.enabled && (a.provider.protocol().is_none() || a.model.is_some()))
        .collect();
    if usable.is_empty() {
        return None;
    }
    let mixed = usable
        .iter()
        .any(|a| a.tool_protocol() == ToolProtocol::Native)
        && usable
            .iter()
            .any(|a| a.tool_protocol() == ToolProtocol::Prompt);
    Some(Route {
        id: RouteId::parse("main").expect("valid route id"),
        strategy: Default::default(),
        steps: usable
            .iter()
            .map(|a| Step {
                account: a.id.clone(),
                model: None,
            })
            .collect(),
        on: Vec::new(),
        switch_at_pct: None,
        allow_mixed_tools: mixed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::handle::Gateway;
    use crate::gateway::testkit::{TWO_ACCOUNTS, rig};

    fn names(snapshot: &GatewaySnapshot) -> Vec<String> {
        snapshot.accounts.iter().map(|a| a.id.to_string()).collect()
    }

    #[test]
    fn moving_an_account_changes_the_order_through_apply_edit() {
        let rig = rig(TWO_ACCOUNTS, Vec::new());
        let before = rig.engine.snapshot();
        assert_eq!(names(&before), ["a", "b"]);
        let (edit, to) = move_account(&before, 0, 1).expect("can move down");
        assert_eq!(to, 1);
        rig.engine.apply_edit(edit).unwrap();
        assert_eq!(names(&rig.engine.snapshot()), ["b", "a"]);
        assert!(
            move_account(&before, 1, 1).is_none(),
            "no row below the last"
        );
        assert!(
            move_account(&before, 0, -1).is_none(),
            "no row above the first"
        );
    }

    #[test]
    fn reordering_steps_ranks_the_accounts_in_the_chain() {
        let rig = rig(TWO_ACCOUNTS, Vec::new());
        let snapshot = rig.engine.snapshot();
        let route = main_route(&snapshot).unwrap().clone();
        let (edit, to) = move_step(&route, 0, 1).unwrap();
        assert_eq!(to, 1);
        rig.engine.apply_edit(edit).unwrap();
        let after = rig.engine.snapshot();
        let steps: Vec<_> = main_route(&after)
            .unwrap()
            .steps
            .iter()
            .map(|s| s.account.to_string())
            .collect();
        assert_eq!(steps, ["b", "a"], "the chain now tries b first");
    }

    #[test]
    fn the_last_step_of_a_route_cannot_be_removed() {
        let rig = rig(TWO_ACCOUNTS, Vec::new());
        let snapshot = rig.engine.snapshot();
        let route = main_route(&snapshot).unwrap().clone();
        let edit = remove_step(&route, 0).unwrap();
        rig.engine.apply_edit(edit).unwrap();
        let after = rig.engine.snapshot();
        let single = main_route(&after).unwrap();
        assert_eq!(single.steps.len(), 1);
        assert!(remove_step(single, 0).is_none());
        assert!(toggle_membership(single, &single.steps[0].account).is_none());
    }

    #[test]
    fn membership_toggles_between_in_and_out() {
        let rig = rig(TWO_ACCOUNTS, Vec::new());
        let snapshot = rig.engine.snapshot();
        let route = main_route(&snapshot).unwrap();
        let b = snapshot.accounts[1].id.clone();
        let edit = toggle_membership(route, &b).unwrap();
        rig.engine.apply_edit(edit).unwrap();
        let after = rig.engine.snapshot();
        assert!(!in_route(main_route(&after).unwrap(), &b));
        let edit = toggle_membership(main_route(&after).unwrap(), &b).unwrap();
        rig.engine.apply_edit(edit).unwrap();
        assert!(in_route(main_route(&rig.engine.snapshot()).unwrap(), &b));
    }

    #[test]
    fn a_switch_off_edit_disables_and_back() {
        let rig = rig(TWO_ACCOUNTS, Vec::new());
        let snapshot = rig.engine.snapshot();
        rig.engine
            .apply_edit(toggle_account(&snapshot, 0).unwrap())
            .unwrap();
        assert!(!rig.engine.snapshot().accounts[0].enabled);
        rig.engine
            .apply_edit(toggle_account(&rig.engine.snapshot(), 0).unwrap())
            .unwrap();
        assert!(rig.engine.snapshot().accounts[0].enabled);
    }

    #[test]
    fn roles_cycle_through_the_routes_and_back_to_inheriting_main() {
        let rig = rig(TWO_ACCOUNTS, Vec::new());
        let snapshot = rig.engine.snapshot();
        // У `title` записи нет: первый шаг вперёд — первый маршрут.
        let ConfigEdit::SetRole { route, .. } = cycle_role(&snapshot, Role::Title, true).unwrap()
        else {
            panic!("a role edit");
        };
        assert_eq!(route.map(|r| r.to_string()).as_deref(), Some("main"));
        // У `main` пустой роли нет: из единственного маршрута цикл возвращает его же.
        let ConfigEdit::SetRole { route, .. } = cycle_role(&snapshot, Role::Main, true).unwrap()
        else {
            panic!("a role edit");
        };
        assert_eq!(route.map(|r| r.to_string()).as_deref(), Some("main"));
    }

    #[test]
    fn a_starter_route_skips_api_accounts_with_no_model() {
        use crate::gateway::types::{Account, AccountProvider};
        let ok = {
            let mut a = Account::new(AccountId::parse("ok").unwrap(), AccountProvider::Openai);
            a.model = Some("m".into());
            a
        };
        let no_model = Account::new(AccountId::parse("nm").unwrap(), AccountProvider::Openai);
        let cli = Account::new(AccountId::parse("cli").unwrap(), AccountProvider::CliClaude);
        let route = starter_route(&[ok, no_model, cli]).unwrap();
        let ids: Vec<_> = route.steps.iter().map(|s| s.account.to_string()).collect();
        assert_eq!(ids, ["ok", "cli"]);
        assert!(starter_route(&[]).is_none());
    }
}
