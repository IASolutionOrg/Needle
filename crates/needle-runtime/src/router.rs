use needle_core::Route;
use thiserror::Error;

#[derive(Debug, Error, Clone, Eq, PartialEq)]
pub enum RouteError {
    #[error("route configuration is ambiguous between `{0}` and `{1}`")]
    Conflict(String, String),
}

pub fn select_route(
    routes: &[Route],
    platform: &str,
    main_model: &str,
    repository: &str,
    need_key: &str,
) -> Result<Option<Route>, RouteError> {
    let mut matching = routes
        .iter()
        .filter(|route| route.enabled && route.matcher.need_key.as_str() == need_key)
        .filter_map(|route| {
            let platform_score = selector_score(&route.matcher.platform, platform)?;
            let model_score = selector_score(&route.matcher.main_model, main_model)?;
            let repository_score = selector_score(&route.matcher.repository, repository)?;
            Some((repository_score, platform_score, model_score, route.priority, route))
        })
        .collect::<Vec<_>>();
    matching.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then(right.1.cmp(&left.1))
            .then(right.2.cmp(&left.2))
            .then(right.3.cmp(&left.3))
            .then(left.4.id.as_bytes().cmp(right.4.id.as_bytes()))
    });
    let Some(best) = matching.first() else {
        return Ok(None);
    };
    if let Some(second) = matching.get(1)
        && best.0 == second.0
        && best.1 == second.1
        && best.2 == second.2
        && best.3 == second.3
        && (best.4.preset_id != second.4.preset_id
            || best.4.definition_digest != second.4.definition_digest)
    {
        return Err(RouteError::Conflict(best.4.id.clone(), second.4.id.clone()));
    }
    Ok(Some(best.4.clone()))
}

fn selector_score(selector: &str, actual: &str) -> Option<u8> {
    match selector {
        "*" => Some(0),
        exact if exact == actual => Some(1),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use needle_core::{NeedKey, RouteMatcher};

    fn route(id: &str, model: &str, repository: &str, priority: i32) -> Route {
        Route::new(
            id,
            priority,
            RouteMatcher {
                platform: "codex".to_owned(),
                main_model: model.to_owned(),
                need_key: NeedKey::new("trace.state-flow").unwrap(),
                repository: repository.to_owned(),
            },
            "trace.state-flow",
        )
    }

    #[test]
    fn exact_repository_and_model_beat_wildcards() {
        let routes = vec![route("wild", "*", "*", 100), route("exact", "gpt", "repo", 0)];
        let selected = select_route(&routes, "codex", "gpt", "repo", "trace.state-flow").unwrap();
        assert_eq!(selected.unwrap().id, "exact");
    }
}
