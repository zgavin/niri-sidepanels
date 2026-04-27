use niri_ipc::Window;

use crate::config::{Config, Side, WindowRule};

fn matches_window(app_id: &Option<String>, title: &Option<String>, rule: &WindowRule) -> bool {
    let app_ok = match (&rule.app_id, app_id) {
        (None, _) => true,
        (Some(re), Some(id)) => re.is_match(id),
        (Some(_), None) => false,
    };

    let title_ok = match (&rule.title, title) {
        (None, _) => true,
        (Some(re), Some(title)) => re.is_match(title),
        (Some(_), None) => false,
    };

    title_ok && app_ok
}

pub fn resolve_window_size(
    rules: &[WindowRule],
    window: &Window,
    default_w: i32,
    default_h: i32,
) -> (i32, i32) {
    for rule in rules {
        if matches_window(&window.app_id, &window.title, rule) {
            return (
                rule.width.unwrap_or(default_w),
                rule.height.unwrap_or(default_h),
            );
        }
    }
    (default_w, default_h)
}

pub fn resolve_rule_peek(rules: &[WindowRule], window: &Window, default_peek: i32) -> i32 {
    for rule in rules {
        if matches_window(&window.app_id, &window.title, rule) {
            return rule.peek.unwrap_or(default_peek);
        }
    }
    default_peek
}

pub fn resolve_rule_focus_peek(
    rules: &[WindowRule],
    window: &Window,
    default_focus_peek: i32,
) -> i32 {
    for rule in rules {
        if matches_window(&window.app_id, &window.title, rule) {
            return rule.focus_peek.unwrap_or(default_focus_peek);
        }
    }
    default_focus_peek
}

/// Return the side to auto-add this window to, if any rule says so.
/// If the matching rule names a `side`, we use it (but only if that panel is
/// enabled — explicit targeting of a disabled panel is treated as "don't
/// auto-add" rather than silently routing somewhere else). Otherwise we fall
/// back to the right panel if enabled, else left, else None.
pub fn resolve_auto_add_side(config: &Config, window: &Window) -> Option<Side> {
    for rule in &config.window_rule {
        if matches_window(&window.app_id, &window.title, rule) && rule.auto_add {
            if let Some(side) = rule.side {
                return if config.panel(side).enabled {
                    Some(side)
                } else {
                    None
                };
            }
            if config.right.enabled {
                return Some(Side::Right);
            }
            if config.left.enabled {
                return Some(Side::Left);
            }
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Panel};
    use crate::test_utils::mock_window;
    use regex::Regex;

    #[test]
    fn test_resolve_window_size_defaults() {
        let rules = vec![];
        let window = mock_window(1, false, false, 1, Some((1.0, 2.0)));
        let (w, h) = resolve_window_size(&rules, &window, 100, 200);
        assert_eq!(w, 100);
        assert_eq!(h, 200);
    }

    #[test]
    fn test_resolve_window_size_match_app_id() {
        let rules = vec![WindowRule {
            app_id: Some(Regex::new("test").unwrap()),
            width: Some(500),
            height: Some(600),
            ..Default::default()
        }];
        let window = mock_window(1, false, false, 1, Some((1.0, 2.0)));
        let (w, h) = resolve_window_size(&rules, &window, 100, 200);
        assert_eq!(w, 500);
        assert_eq!(h, 600);
    }

    #[test]
    fn test_resolve_rule_peek_match() {
        let rules = vec![WindowRule {
            app_id: Some(Regex::new("test").unwrap()),
            peek: Some(50),
            ..Default::default()
        }];
        let window = mock_window(1, false, false, 1, Some((1.0, 2.0)));
        assert_eq!(resolve_rule_peek(&rules, &window, 10), 50);
    }

    #[test]
    fn test_resolve_rule_focus_peek_match() {
        let rules = vec![WindowRule {
            app_id: Some(Regex::new("test").unwrap()),
            focus_peek: Some(70),
            ..Default::default()
        }];
        let window = mock_window(1, false, false, 1, Some((1.0, 2.0)));
        assert_eq!(resolve_rule_focus_peek(&rules, &window, 20), 70);
    }

    #[test]
    fn test_resolve_auto_add_side_explicit() {
        // Given: a rule explicitly targeting `left`, with the left panel enabled.
        let mut config = Config {
            window_rule: vec![WindowRule {
                app_id: Some(Regex::new("test").unwrap()),
                auto_add: true,
                side: Some(Side::Left),
                ..Default::default()
            }],
            ..Default::default()
        };
        config.left = Panel {
            enabled: true,
            ..Panel::default()
        };
        let window = mock_window(1, false, false, 1, None);

        // When: we resolve the auto-add side.
        let resolved = resolve_auto_add_side(&config, &window);

        // Then: the rule's explicit side wins.
        assert_eq!(resolved, Some(Side::Left));
    }

    #[test]
    fn test_resolve_auto_add_side_returns_none_if_explicit_side_disabled() {
        // Given: a rule explicitly targeting `left`, but left is disabled.
        let config = Config {
            window_rule: vec![WindowRule {
                app_id: Some(Regex::new("test").unwrap()),
                auto_add: true,
                side: Some(Side::Left),
                ..Default::default()
            }],
            // left defaults to enabled = false
            ..Default::default()
        };
        let window = mock_window(1, false, false, 1, None);

        // When: we resolve the auto-add side.
        let resolved = resolve_auto_add_side(&config, &window);

        // Then: we treat this as "don't auto-add" rather than silently routing
        // to the enabled fallback. Surfaces the config bug to the user.
        assert_eq!(resolved, None);
    }

    #[test]
    fn test_resolve_auto_add_side_defaults_to_right_if_enabled() {
        let config = Config {
            window_rule: vec![WindowRule {
                app_id: Some(Regex::new("test").unwrap()),
                auto_add: true,
                side: None,
                ..Default::default()
            }],
            ..Default::default()
        };
        let window = mock_window(1, false, false, 1, None);
        assert_eq!(resolve_auto_add_side(&config, &window), Some(Side::Right));
    }

    #[test]
    fn test_resolve_auto_add_side_falls_back_to_left_if_right_disabled() {
        let mut config = Config {
            window_rule: vec![WindowRule {
                app_id: Some(Regex::new("test").unwrap()),
                auto_add: true,
                side: None,
                ..Default::default()
            }],
            ..Default::default()
        };
        config.right.enabled = false;
        config.left = Panel {
            enabled: true,
            ..Panel::default()
        };
        let window = mock_window(1, false, false, 1, None);
        assert_eq!(resolve_auto_add_side(&config, &window), Some(Side::Left));
    }

    #[test]
    fn test_resolve_auto_add_side_none_if_no_rule_or_not_enabled() {
        let config = Config::default();
        let window = mock_window(1, false, false, 1, None);
        assert_eq!(resolve_auto_add_side(&config, &window), None);
    }
}
