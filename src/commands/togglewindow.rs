use crate::Ctx;
use crate::commands::reorder;
use crate::config::Side;
use crate::niri::NiriClient;
use crate::state::{WindowState, save_state};
use crate::window_rules::resolve_window_size;
use anyhow::Result;
use niri_ipc::{Action, SizeChange, Window};

pub fn toggle_window<C: NiriClient>(ctx: &mut Ctx<C>, side: Side) -> Result<()> {
    let focused = ctx.socket.get_active_window()?;

    match ctx.state.side_of(focused.id) {
        Some(existing) if existing == side => {
            // Already on this side — remove it.
            remove_from_panel(ctx, existing, &focused)?;
        }
        Some(other) => {
            // On the other panel — move it over.
            remove_from_panel(ctx, other, &focused)?;
            add_to_panel(ctx, side, &focused)?;
        }
        None => {
            add_to_panel(ctx, side, &focused)?;
        }
    }

    save_state(&ctx.state, &ctx.cache_dir)?;
    reorder(ctx)?;

    Ok(())
}

pub fn add_to_panel<C: NiriClient>(ctx: &mut Ctx<C>, side: Side, window: &Window) -> Result<()> {
    let (width, height) = window.layout.window_size;
    let w_state = WindowState {
        id: window.id,
        width,
        height,
        is_floating: window.is_floating,
        position: window.layout.tile_pos_in_workspace_view,
    };
    ctx.state.panel_mut(side).windows.push(w_state);

    if !window.is_floating {
        let _ = ctx.socket.send_action(Action::ToggleWindowFloating {
            id: Some(window.id),
        });
    }

    let panel = ctx.config.panel(side);
    let (target_width, target_height) =
        resolve_window_size(&ctx.config.window_rule, window, panel.width, panel.height);

    let _ = ctx.socket.send_action(Action::SetWindowWidth {
        change: SizeChange::SetFixed(target_width),
        id: Some(window.id),
    });

    let _ = ctx.socket.send_action(Action::SetWindowHeight {
        change: SizeChange::SetFixed(target_height),
        id: Some(window.id),
    });

    Ok(())
}

pub(crate) fn remove_from_panel<C: NiriClient>(
    ctx: &mut Ctx<C>,
    side: Side,
    window: &Window,
) -> Result<()> {
    let panel_state = ctx.state.panel_mut(side);
    let index = panel_state
        .windows
        .iter()
        .position(|w| w.id == window.id)
        .expect("remove_from_panel called with a window not on that side");

    let w_state = panel_state.windows.remove(index);
    ctx.state.ignored_windows.push(w_state.id);

    let _ = ctx.socket.send_action(Action::SetWindowWidth {
        change: SizeChange::SetFixed(w_state.width),
        id: Some(window.id),
    });

    let _ = ctx.socket.send_action(Action::SetWindowHeight {
        change: SizeChange::SetFixed(w_state.height),
        id: Some(window.id),
    });

    if window.is_floating && !w_state.is_floating {
        let _ = ctx.socket.send_action(Action::ToggleWindowFloating {
            id: Some(window.id),
        });
    }

    if let Some((x, y)) = w_state.position
        && window.is_floating
    {
        let _ = ctx.socket.send_action(Action::MoveFloatingWindow {
            id: Some(window.id),
            x: niri_ipc::PositionChange::SetFixed(x),
            y: niri_ipc::PositionChange::SetFixed(y),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use niri_ipc::PositionChange;
    use tempfile::tempdir;

    use super::*;
    use crate::config::WindowRule;
    use crate::state::AppState;
    use crate::test_utils::{MockNiri, mock_config, mock_window};

    #[test]
    fn test_add_to_right_panel_tiled() {
        let temp_dir = tempdir().unwrap();
        let win = mock_window(100, true, false, 1, None);
        let mock = MockNiri::new(vec![win]);

        let mut ctx = Ctx {
            state: AppState::default(),
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        toggle_window(&mut ctx, Side::Right).expect("Command failed");

        assert_eq!(ctx.state.right.windows.len(), 1);
        assert!(ctx.state.left.windows.is_empty());
        let w_state = &ctx.state.right.windows[0];
        assert_eq!(w_state.id, 100);
        assert_eq!(w_state.width, 1000);
        assert_eq!(w_state.height, 800);

        let actions = &ctx.socket.sent_actions;
        assert!(actions.iter().any(|a| matches!(a, Action::ToggleWindowFloating { id: Some(100) })));
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowWidth { change: SizeChange::SetFixed(300), id: Some(100) }
        )));
    }

    #[test]
    fn test_add_to_left_panel_uses_left_width() {
        let temp_dir = tempdir().unwrap();
        let win = mock_window(100, true, false, 1, None);
        let mock = MockNiri::new(vec![win]);

        let mut config = mock_config();
        config.left.enabled = true;
        config.left.width = 222;
        config.left.height = 444;

        let mut ctx = Ctx {
            state: AppState::default(),
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        toggle_window(&mut ctx, Side::Left).expect("Command failed");

        assert_eq!(ctx.state.left.windows.len(), 1);

        let actions = &ctx.socket.sent_actions;
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowWidth { change: SizeChange::SetFixed(222), id: Some(100) }
        )));
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowHeight { change: SizeChange::SetFixed(444), id: Some(100) }
        )));
    }

    #[test]
    fn test_toggling_same_side_removes() {
        let temp_dir = tempdir().unwrap();
        let win = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![win]);

        let mut state = AppState::default();
        state.right.windows.push(WindowState {
            id: 100,
            width: 1000,
            height: 800,
            is_floating: true,
            position: Some((1.0, 2.0)),
        });

        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        toggle_window(&mut ctx, Side::Right).expect("Command failed");

        assert!(ctx.state.right.windows.is_empty());
        assert!(ctx.state.ignored_windows.contains(&100));

        let actions = &ctx.socket.sent_actions;
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowWidth { change: SizeChange::SetFixed(1000), id: Some(100) }
        )));
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow {
                id: Some(100),
                x: PositionChange::SetFixed(1.0),
                y: PositionChange::SetFixed(2.0)
            }
        )));
    }

    #[test]
    fn test_toggling_other_side_moves_across() {
        let temp_dir = tempdir().unwrap();
        let win = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![win]);

        let mut config = mock_config();
        config.left.enabled = true;

        let mut state = AppState::default();
        state.right.windows.push(WindowState {
            id: 100,
            width: 1000,
            height: 800,
            is_floating: true,
            position: Some((1.0, 2.0)),
        });

        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        toggle_window(&mut ctx, Side::Left).expect("Command failed");

        assert!(ctx.state.right.windows.is_empty(), "removed from right");
        assert_eq!(ctx.state.left.windows.len(), 1, "added to left");
        assert_eq!(ctx.state.left.windows[0].id, 100);
    }

    #[test]
    fn test_add_with_window_rule_overrides_size() {
        let temp_dir = tempdir().unwrap();
        let mut win = mock_window(100, true, false, 1, Some((1.0, 2.0)));
        win.app_id = Some("special".into());
        let mock = MockNiri::new(vec![win]);

        let mut config = mock_config();
        config.window_rule = vec![WindowRule {
            app_id: Some(regex::Regex::new("special").unwrap()),
            width: Some(500),
            height: Some(600),
            ..Default::default()
        }];

        let mut ctx = Ctx {
            state: AppState::default(),
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        toggle_window(&mut ctx, Side::Right).expect("Command failed");

        let actions = &ctx.socket.sent_actions;
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowWidth { change: SizeChange::SetFixed(500), id: Some(100) }
        )));
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowHeight { change: SizeChange::SetFixed(600), id: Some(100) }
        )));
    }
}
