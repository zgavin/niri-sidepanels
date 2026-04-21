use crate::config::{Panel, Side};
use crate::niri::NiriClient;
use crate::state::{PanelState, save_state};
use crate::window_rules::{resolve_rule_focus_peek, resolve_rule_peek, resolve_window_size};
use crate::{Ctx, WindowTarget};
use anyhow::Result;
use niri_ipc::{Action, PositionChange, Window};
use std::collections::HashSet;

fn resolve_dimensions<C: NiriClient>(window: &Window, panel: &Panel, ctx: &Ctx<C>) -> WindowTarget {
    let (width, height) = resolve_window_size(
        &ctx.config.window_rule,
        window,
        panel.width,
        panel.height,
    );
    WindowTarget { width, height }
}

fn calculate_coordinates(
    side: Side,
    panel: &Panel,
    state: &PanelState,
    dims: WindowTarget,
    screen: (i32, i32),
    stack_offset: i32,
    active_peek: i32,
) -> (i32, i32) {
    let (sw, sh) = screen;
    let (w, h) = (dims.width, dims.height);
    let margins = &panel.margins;

    match side {
        Side::Right => {
            let visible_x = sw - w - margins.right;
            let hidden_x = sw - active_peek;
            let x = if state.is_hidden { hidden_x } else { visible_x };

            let start_y = sh - h - margins.bottom;
            let y = start_y - stack_offset;
            (x, y)
        }
        Side::Left => {
            let visible_x = margins.left;
            let hidden_x = -w + active_peek;
            let x = if state.is_hidden { hidden_x } else { visible_x };

            let start_y = sh - h - margins.bottom;
            let y = start_y - stack_offset;
            (x, y)
        }
    }
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
        .filter(|w| {
            w.is_floating && w.workspace_id == Some(current_ws) && ids.contains(&w.id)
        })
        .collect();

    windows.sort_by_key(|w| ids.iter().position(|id| *id == w.id).unwrap_or(usize::MAX));
    if panel_state.is_flipped {
        windows.reverse();
    }

    let panel_gap = panel.gap;
    let mut current_stack_offset = 0;

    for window in windows.iter() {
        let dims = resolve_dimensions(window, panel, ctx);

        let active_peek = if window.is_focused {
            resolve_rule_focus_peek(&ctx.config.window_rule, window, panel.get_focus_peek())
        } else {
            resolve_rule_peek(&ctx.config.window_rule, window, panel.peek)
        };

        let (target_x, target_y) = calculate_coordinates(
            side,
            panel,
            panel_state,
            dims,
            screen,
            current_stack_offset,
            active_peek,
        );

        current_stack_offset += dims.height + panel_gap;

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

    #[test]
    fn test_standard_right_stacking() {
        // Two windows on the right panel. Check Y-axis stacking.
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, Some((1.0, 2.0)));
        let w2 = mock_window(2, true, true, 1, Some((1.0, 2.0)));
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
        assert_eq!(actions.len(), 2);

        // Screen 1920x1080, panel w=300 h=200 gap=10 margins right=20 bottom=50
        let base_x = 1920 - 300 - 20; // 1600
        let base_y = 1080 - 200 - 50; // 830

        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow {
                id: Some(1),
                x: PositionChange::SetFixed(x),
                y: PositionChange::SetFixed(y)
            } if *x == f64::from(base_x) && *y == f64::from(base_y)
        )));

        // Window 2 stacked above: y = base_y - (h + gap) = 830 - 210 = 620
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow {
                id: Some(2),
                x: PositionChange::SetFixed(x),
                y: PositionChange::SetFixed(y)
            } if *x == f64::from(base_x) && *y == 620.0
        )));
    }

    #[test]
    fn test_right_hidden_with_focus_peek() {
        // In hidden mode, focused window peeks further than unfocused.
        let temp_dir = tempdir().unwrap();
        let w_focused = mock_window(1, true, true, 1, Some((1.0, 2.0)));
        let w_bg = mock_window(2, false, true, 1, Some((1.0, 2.0)));
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

        // peek=10, focus_peek=50, screen_w=1920
        // Unfocused (id=2): x = 1920 - 10 = 1910
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
    fn test_filters_wrong_workspace_and_cleans_zombies() {
        // w1 on ws 1, w2 on ws 99, w3 in state but not in niri.
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, Some((1.0, 2.0)));
        let w2 = mock_window(2, false, true, 99, Some((1.0, 2.0)));
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

        // Zombie (id=3) dropped, others kept.
        let ids: Vec<u64> = ctx.state.right.windows.iter().map(|w| w.id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        assert!(!ids.contains(&3));

        let actions = &ctx.socket.sent_actions;
        assert!(actions.iter().any(|a| matches!(a, Action::MoveFloatingWindow { id: Some(1), .. })));
        assert!(!actions.iter().any(|a| matches!(a, Action::MoveFloatingWindow { id: Some(2), .. })));
        assert!(!actions.iter().any(|a| matches!(a, Action::MoveFloatingWindow { id: Some(3), .. })));
    }

    #[test]
    fn test_flipped_order() {
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, Some((1.0, 2.0)));
        let w2 = mock_window(2, false, true, 1, Some((1.0, 2.0)));
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

        let actions = &ctx.socket.sent_actions;
        // With flip: id=2 is bottom (830), id=1 is stacked above (620).
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow { id: Some(2), y: PositionChange::SetFixed(y), .. }
            if *y == 830.0
        )));
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow { id: Some(1), y: PositionChange::SetFixed(y), .. }
            if *y == 620.0
        )));
    }

    #[test]
    fn test_left_panel_hidden() {
        // Left panel, hidden. width=300, peek=10, margin.left=0 → x = -300 + 10 = -290.
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w1]);

        let mut config = mock_config();
        // Turn off right, enable left with same dims, explicit margin.left=0.
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
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow { id: Some(1), x: PositionChange::SetFixed(x), .. }
            if *x == -290.0
        )));
    }

    #[test]
    fn test_both_panels_independent() {
        // Left has id=10, right has id=20; both enabled.
        let temp_dir = tempdir().unwrap();
        let w_left = mock_window(10, false, true, 1, Some((1.0, 2.0)));
        let w_right = mock_window(20, false, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w_left, w_right]);

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
            right: panel_state_with(vec![win_state(20)]),
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

        // Left window: visible_x = margin.left = 0
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow { id: Some(10), x: PositionChange::SetFixed(x), .. }
            if *x == 0.0
        )));

        // Right window: visible_x = 1920 - 300 - 20 = 1600
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow { id: Some(20), x: PositionChange::SetFixed(x), .. }
            if *x == 1600.0
        )));
    }

    #[test]
    fn test_disabled_panel_is_skipped() {
        // Right panel disabled, but has stale window in state; no actions for it.
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, Some((1.0, 2.0)));
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

    #[test]
    fn test_window_rules_override_sizes_on_right() {
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, Some((1.0, 2.0)));
        let mut w2 = mock_window(2, false, true, 1, Some((1.0, 2.0)));
        w2.app_id = Some("special".into());
        let mock = MockNiri::new(vec![w1, w2]);

        let mut config = mock_config();
        config.window_rule = vec![WindowRule {
            app_id: Some(Regex::new("special").unwrap()),
            width: Some(500),
            peek: Some(100),
            ..Default::default()
        }];

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
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).expect("Reorder failed");

        let actions = &ctx.socket.sent_actions;
        // Default: hidden_x = 1920 - peek(10) = 1910
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow { id: Some(1), x: PositionChange::SetFixed(x), .. }
            if *x == 1910.0
        )));
        // Special: hidden_x = 1920 - peek(100) = 1820
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow { id: Some(2), x: PositionChange::SetFixed(x), .. }
            if *x == 1820.0
        )));
    }
}
