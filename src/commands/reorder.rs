use crate::config::{Panel, Side};
use crate::niri::NiriClient;
use crate::state::{PanelState, save_state};
use crate::window_rules::{resolve_rule_focus_peek, resolve_rule_peek, resolve_window_size};
use crate::{Ctx, WindowTarget};
use anyhow::Result;
use niri_ipc::{Action, PositionChange, SizeChange, Window};
use std::collections::HashSet;

/// Never shrink a panel window below this height even if many windows are
/// stacked. Prevents division producing unusable or negative heights.
const MIN_WINDOW_HEIGHT: i32 = 80;

fn resolve_width<C: NiriClient>(window: &Window, panel: &Panel, ctx: &Ctx<C>) -> i32 {
    let (width, _) = resolve_window_size(
        &ctx.config.window_rule,
        window,
        panel.width,
        panel.height,
    );
    width
}

fn calculate_coordinates(
    side: Side,
    panel: &Panel,
    state: &PanelState,
    dims: WindowTarget,
    screen: (i32, i32),
    y: i32,
    active_peek: i32,
) -> (i32, i32) {
    let (sw, _) = screen;
    let w = dims.width;
    let margins = &panel.margins;

    match side {
        Side::Right => {
            let visible_x = sw - w - margins.right;
            let hidden_x = sw - active_peek;
            let x = if state.is_hidden { hidden_x } else { visible_x };
            (x, y)
        }
        Side::Left => {
            let visible_x = margins.left;
            let hidden_x = -w + active_peek;
            let x = if state.is_hidden { hidden_x } else { visible_x };
            (x, y)
        }
    }
}

/// Divide the panel's available vertical space among `n` windows with `gap`
/// pixels between them. Returns the per-window height, clamped to
/// [MIN_WINDOW_HEIGHT, available_height].
fn equal_height(screen_h: i32, margins_top: i32, margins_bottom: i32, gap: i32, n: usize) -> i32 {
    if n == 0 {
        return 0;
    }
    let available = screen_h - margins_top - margins_bottom;
    let total_gap = gap * (n as i32 - 1);
    let per = (available - total_gap) / n as i32;
    per.max(MIN_WINDOW_HEIGHT)
}

pub fn reorder<C: NiriClient>(ctx: &mut Ctx<C>) -> Result<()> {
    let (display_w, display_h) = ctx.socket.get_screen_dimensions()?;
    let current_ws = ctx.socket.get_active_workspace()?.id;
    let all_windows = ctx.socket.get_windows()?;

    // Garbage-collect windows that no longer exist in niri.
    let active_ids: HashSet<u64> = all_windows.iter().map(|w| w.id).collect();
    let mut dirty = false;
    for side in Side::ALL {
        let panel_state = ctx.state.panel_mut(side);
        let before = panel_state.windows.len();
        panel_state.windows.retain(|w| active_ids.contains(&w.id));
        if panel_state.windows.len() != before {
            dirty = true;
        }
    }
    if dirty {
        save_state(&ctx.state, &ctx.cache_dir)?;
    }

    for side in Side::ALL {
        reorder_side(ctx, side, &all_windows, current_ws, (display_w, display_h))?;
    }

    Ok(())
}

fn reorder_side<C: NiriClient>(
    ctx: &mut Ctx<C>,
    side: Side,
    all_windows: &[Window],
    current_ws: u64,
    screen: (i32, i32),
) -> Result<()> {
    let panel = ctx.config.panel(side);
    if !panel.enabled {
        return Ok(());
    }

    let panel_state = ctx.state.panel(side);
    let ids: Vec<u64> = panel_state.windows.iter().map(|w| w.id).collect();

    let mut windows: Vec<_> = all_windows
        .iter()
        .filter(|w| w.is_floating && w.workspace_id == Some(current_ws) && ids.contains(&w.id))
        .collect();

    windows.sort_by_key(|w| ids.iter().position(|id| *id == w.id).unwrap_or(usize::MAX));
    if panel_state.is_flipped {
        windows.reverse();
    }

    let n = windows.len();
    if n == 0 {
        return Ok(());
    }

    let (_, screen_h) = screen;
    let gap = panel.gap;
    let per_height = equal_height(screen_h, panel.margins.top, panel.margins.bottom, gap, n);

    // Layout bottom-up: first window at the bottom, subsequent windows stacked
    // above with `gap` between them.
    for (i, window) in windows.iter().enumerate() {
        let width = resolve_width(window, panel, ctx);
        let dims = WindowTarget { width, height: per_height };

        let active_peek = if window.is_focused {
            resolve_rule_focus_peek(&ctx.config.window_rule, window, panel.get_focus_peek())
        } else {
            resolve_rule_peek(&ctx.config.window_rule, window, panel.peek)
        };

        let stack_y = screen_h
            - panel.margins.bottom
            - per_height
            - (i as i32) * (per_height + gap);

        let (target_x, target_y) = calculate_coordinates(
            side,
            panel,
            panel_state,
            dims,
            screen,
            stack_y,
            active_peek,
        );

        // Size each panel window to the computed per-window height + resolved
        // width. niri ignores redundant size requests so this is cheap on
        // unchanged layouts.
        let _ = ctx.socket.send_action(Action::SetWindowWidth {
            change: SizeChange::SetFixed(width),
            id: Some(window.id),
        });
        let _ = ctx.socket.send_action(Action::SetWindowHeight {
            change: SizeChange::SetFixed(per_height),
            id: Some(window.id),
        });

        let _ = ctx.socket.send_action(Action::MoveFloatingWindow {
            id: Some(window.id),
            x: PositionChange::SetFixed(target_x.into()),
            y: PositionChange::SetFixed(target_y.into()),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WindowRule;
    use crate::state::{AppState, WindowState};
    use crate::test_utils::{MockNiri, mock_config, mock_window};
    use niri_ipc::{Action, PositionChange};
    use regex::Regex;
    use tempfile::tempdir;

    fn panel_state_with(windows: Vec<WindowState>) -> PanelState {
        PanelState {
            windows,
            is_hidden: false,
            is_flipped: false,
        }
    }

    fn win_state(id: u64) -> WindowState {
        WindowState {
            id,
            width: 300,
            height: 200,
            is_floating: false,
            position: None,
        }
    }

    // mock_config: screen 1920x1080, right panel width=300 gap=10 margins
    // top=50 right=20 left=10 bottom=50. Usable vertical = 1080-50-50 = 980.

    fn find_move_y(actions: &[Action], id: u64) -> Option<f64> {
        actions.iter().find_map(|a| match a {
            Action::MoveFloatingWindow {
                id: Some(aid),
                y: PositionChange::SetFixed(y),
                ..
            } if *aid == id => Some(*y),
            _ => None,
        })
    }

    #[test]
    fn test_equal_height_one_window_fills_available() {
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, None);
        let mock = MockNiri::new(vec![w1]);

        let state = AppState {
            right: panel_state_with(vec![win_state(1)]),
            ..Default::default()
        };

        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).expect("Reorder failed");

        let actions = &ctx.socket.sent_actions;
        // N=1 → per_height = 980 (entire usable vertical)
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowHeight { change: SizeChange::SetFixed(980), id: Some(1) }
        )));
        // stack_y = 1080 - 50 - 980 - 0 = 50
        assert_eq!(find_move_y(actions, 1), Some(50.0));
    }

    #[test]
    fn test_equal_height_two_windows_divide_equally() {
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, None);
        let w2 = mock_window(2, false, true, 1, None);
        let mock = MockNiri::new(vec![w1, w2]);

        let state = AppState {
            right: panel_state_with(vec![win_state(1), win_state(2)]),
            ..Default::default()
        };

        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).expect("Reorder failed");

        let actions = &ctx.socket.sent_actions;
        // N=2: per_height = (980 - 10) / 2 = 485
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowHeight { change: SizeChange::SetFixed(485), id: Some(1) }
        )));
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowHeight { change: SizeChange::SetFixed(485), id: Some(2) }
        )));
        // Bottom window (id=1): stack_y = 1080 - 50 - 485 = 545
        // Top window (id=2): stack_y = 545 - 10 - 485 = 50
        assert_eq!(find_move_y(actions, 1), Some(545.0));
        assert_eq!(find_move_y(actions, 2), Some(50.0));
    }

    #[test]
    fn test_equal_height_many_windows_clamped_to_min() {
        let temp_dir = tempdir().unwrap();
        // 20 windows with gap 10 would normally give tiny heights; clamp to min.
        let mut windows = vec![];
        let mut state_windows = vec![];
        for i in 1..=20 {
            windows.push(mock_window(i, false, true, 1, None));
            state_windows.push(win_state(i));
        }
        let mock = MockNiri::new(windows);

        let state = AppState {
            right: panel_state_with(state_windows),
            ..Default::default()
        };

        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).expect("Reorder failed");
        // (980 - 19*10) / 20 = 790/20 = 39.5 → 39, then clamped to MIN_WINDOW_HEIGHT = 80
        let actions = &ctx.socket.sent_actions;
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowHeight { change: SizeChange::SetFixed(80), id: Some(1) }
        )));
    }

    #[test]
    fn test_right_hidden_with_focus_peek_still_respects_equal_height() {
        let temp_dir = tempdir().unwrap();
        let w_focused = mock_window(1, true, true, 1, None);
        let w_bg = mock_window(2, false, true, 1, None);
        let mock = MockNiri::new(vec![w_focused, w_bg]);

        let state = AppState {
            right: PanelState {
                windows: vec![win_state(1), win_state(2)],
                is_hidden: true,
                is_flipped: false,
            },
            ..Default::default()
        };

        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).expect("Reorder failed");

        let actions = &ctx.socket.sent_actions;
        // Unfocused (id=2): x = 1920 - peek(10) = 1910
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow { id: Some(2), x: PositionChange::SetFixed(x), .. }
            if *x == 1910.0
        )));
        // Focused (id=1): x = 1920 - 50 = 1870
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow { id: Some(1), x: PositionChange::SetFixed(x), .. }
            if *x == 1870.0
        )));
    }

    #[test]
    fn test_gc_retains_only_active_windows() {
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, None);
        let w2 = mock_window(2, false, true, 99, None); // wrong workspace
        let mock = MockNiri::new(vec![w1, w2]);

        let state = AppState {
            right: panel_state_with(vec![win_state(1), win_state(2), win_state(3)]),
            ..Default::default()
        };

        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).unwrap();

        // Zombie (id=3) dropped, id=1 and id=2 kept in state.
        let ids: Vec<u64> = ctx.state.right.windows.iter().map(|w| w.id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        assert!(!ids.contains(&3));

        // Only id=1 actually gets reordered (id=2 is on a different ws).
        let actions = &ctx.socket.sent_actions;
        assert!(actions.iter().any(|a| matches!(a, Action::MoveFloatingWindow { id: Some(1), .. })));
        assert!(!actions.iter().any(|a| matches!(a, Action::MoveFloatingWindow { id: Some(2), .. })));
    }

    #[test]
    fn test_flipped_order_inverts_stack() {
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, None);
        let w2 = mock_window(2, false, true, 1, None);
        let mock = MockNiri::new(vec![w1, w2]);

        let state = AppState {
            right: PanelState {
                windows: vec![win_state(1), win_state(2)],
                is_hidden: false,
                is_flipped: true,
            },
            ..Default::default()
        };

        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).unwrap();

        // With flip, id=2 is at the bottom slot, id=1 above.
        // per_height = 485, bottom_y = 545, top_y = 50
        assert_eq!(find_move_y(&ctx.socket.sent_actions, 2), Some(545.0));
        assert_eq!(find_move_y(&ctx.socket.sent_actions, 1), Some(50.0));
    }

    #[test]
    fn test_left_panel_hidden_geometry_unchanged() {
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, None);
        let mock = MockNiri::new(vec![w1]);

        let mut config = mock_config();
        config.left = Panel {
            enabled: true,
            width: 300,
            height: 200,
            gap: 10,
            peek: 10,
            focus_peek: Some(50),
            sticky: false,
            margins: crate::config::Margins {
                top: 50,
                right: 0,
                left: 0,
                bottom: 50,
            },
        };
        config.right.enabled = false;

        let state = AppState {
            left: PanelState {
                windows: vec![win_state(1)],
                is_hidden: true,
                is_flipped: false,
            },
            ..Default::default()
        };

        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).expect("Reorder failed");
        let actions = &ctx.socket.sent_actions;
        // width=300, peek=10 → x = -300 + 10 = -290
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow { id: Some(1), x: PositionChange::SetFixed(x), .. }
            if *x == -290.0
        )));
    }

    #[test]
    fn test_both_panels_independent_heights() {
        let temp_dir = tempdir().unwrap();
        // Left has 1 window (height=980), right has 2 (height=485 each).
        let w_left = mock_window(10, false, true, 1, None);
        let w_r1 = mock_window(20, false, true, 1, None);
        let w_r2 = mock_window(21, false, true, 1, None);
        let mock = MockNiri::new(vec![w_left, w_r1, w_r2]);

        let mut config = mock_config();
        config.left = Panel {
            enabled: true,
            width: 300,
            height: 200,
            gap: 10,
            peek: 10,
            focus_peek: Some(50),
            sticky: false,
            margins: crate::config::Margins {
                top: 50,
                right: 0,
                left: 0,
                bottom: 50,
            },
        };

        let state = AppState {
            left: panel_state_with(vec![win_state(10)]),
            right: panel_state_with(vec![win_state(20), win_state(21)]),
            ..Default::default()
        };

        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).unwrap();
        let actions = &ctx.socket.sent_actions;

        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowHeight { change: SizeChange::SetFixed(980), id: Some(10) }
        )));
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowHeight { change: SizeChange::SetFixed(485), id: Some(20) }
        )));
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowHeight { change: SizeChange::SetFixed(485), id: Some(21) }
        )));
    }

    #[test]
    fn test_window_rules_still_override_width_not_height() {
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, None);
        let mut w2 = mock_window(2, false, true, 1, None);
        w2.app_id = Some("wide".into());
        let mock = MockNiri::new(vec![w1, w2]);

        let mut config = mock_config();
        config.window_rule = vec![WindowRule {
            app_id: Some(Regex::new("wide").unwrap()),
            width: Some(500),
            height: Some(600), // should be ignored — equal-height overrides
            ..Default::default()
        }];

        let state = AppState {
            right: panel_state_with(vec![win_state(1), win_state(2)]),
            ..Default::default()
        };

        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).unwrap();
        let actions = &ctx.socket.sent_actions;

        // id=2: width=500 (rule), height=485 (equal-height, NOT 600 from rule).
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowWidth { change: SizeChange::SetFixed(500), id: Some(2) }
        )));
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowHeight { change: SizeChange::SetFixed(485), id: Some(2) }
        )));
        // id=1 uses panel default width=300.
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowWidth { change: SizeChange::SetFixed(300), id: Some(1) }
        )));
    }

    #[test]
    fn test_configurable_gap_affects_stacking() {
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, None);
        let w2 = mock_window(2, false, true, 1, None);
        let mock = MockNiri::new(vec![w1, w2]);

        let mut config = mock_config();
        config.right.gap = 50; // bigger gap

        let state = AppState {
            right: panel_state_with(vec![win_state(1), win_state(2)]),
            ..Default::default()
        };

        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).unwrap();
        let actions = &ctx.socket.sent_actions;
        // N=2, gap=50: per_height = (980 - 50) / 2 = 465
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowHeight { change: SizeChange::SetFixed(465), id: Some(1) }
        )));
        // bottom: 1080 - 50 - 465 = 565
        // top: 565 - 50 - 465 = 50
        assert_eq!(find_move_y(actions, 1), Some(565.0));
        assert_eq!(find_move_y(actions, 2), Some(50.0));
    }

    #[test]
    fn test_disabled_panel_is_skipped() {
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, None);
        let mock = MockNiri::new(vec![w1]);

        let mut config = mock_config();
        config.right.enabled = false;

        let state = AppState {
            right: panel_state_with(vec![win_state(1)]),
            ..Default::default()
        };

        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).unwrap();
        assert!(ctx.socket.sent_actions.is_empty());
    }
}
