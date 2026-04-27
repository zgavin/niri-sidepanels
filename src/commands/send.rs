use crate::Ctx;
use crate::commands::reorder;
use crate::commands::togglewindow::{add_to_panel, remove_to_floating, remove_to_tape};
use crate::config::Side;
use crate::niri::NiriClient;
use crate::state::save_state;
use anyhow::Result;
use clap::ValueEnum;
use niri_ipc::Action;

/// Where to send the focused window.
///
/// `Left` / `Right` place the window on that panel (moving it across if it's
/// already on the other panel). `Center` un-floats the window and returns it
/// to the niri tape — use this as the "put this back where it belongs" button.
/// `Floating` detaches the window from panel tracking but leaves it floating
/// exactly where it is, at its current size — useful for popping a window out
/// of its slot without fully committing it back to the tape.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum Target {
    Left,
    Right,
    Center,
    Floating,
}

enum Destination {
    Panel(Side),
    Tape,
    Floating,
}

impl Target {
    fn destination(self) -> Destination {
        match self {
            Target::Left => Destination::Panel(Side::Left),
            Target::Right => Destination::Panel(Side::Right),
            Target::Center => Destination::Tape,
            Target::Floating => Destination::Floating,
        }
    }
}

pub fn send<C: NiriClient>(ctx: &mut Ctx<C>, target: Target) -> Result<()> {
    if let Destination::Panel(side) = target.destination() {
        ctx.config.require_enabled(side)?;
    }
    let focused = ctx.socket.get_active_window()?;
    let current = ctx.state.side_of(focused.id);

    match (current, target.destination()) {
        (Some(current_side), Destination::Panel(target_side)) if current_side == target_side => {
            // Already on target panel — no state change.
        }
        (Some(current_side), Destination::Panel(target_side)) => {
            // Panel → other panel. add_to_panel will take over sizing, so just
            // drop tracking on the source side without fighting a restore.
            remove_to_floating(ctx, current_side, &focused)?;
            add_to_panel(ctx, target_side, &focused)?;
            // The removal pushed the id into ignored_windows; undo so the
            // listener doesn't skip the add's follow-up events.
            ctx.state.ignored_windows.retain(|id| *id != focused.id);
        }
        (Some(current_side), Destination::Tape) => {
            remove_to_tape(ctx, current_side, &focused)?;
        }
        (Some(current_side), Destination::Floating) => {
            remove_to_floating(ctx, current_side, &focused)?;
        }
        (None, Destination::Panel(target_side)) => {
            add_to_panel(ctx, target_side, &focused)?;
        }
        (None, Destination::Tape) => {
            // Untracked but currently floating — un-float to put it back in
            // the tape. Already-tiled windows are a no-op.
            if focused.is_floating {
                let _ = ctx.socket.send_action(Action::ToggleWindowFloating {
                    id: Some(focused.id),
                });
            }
        }
        (None, Destination::Floating) => {
            // Untracked but currently tiled — float it. Already-floating
            // windows are a no-op.
            if !focused.is_floating {
                let _ = ctx.socket.send_action(Action::ToggleWindowFloating {
                    id: Some(focused.id),
                });
            }
        }
    }

    save_state(&ctx.state, &ctx.cache_dir)?;
    reorder(ctx)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppState, WindowState};
    use crate::test_utils::{MockNiri, mock_config, mock_window};
    use niri_ipc::{Action, SizeChange};
    use tempfile::tempdir;

    fn ws(id: u64, w: i32, h: i32) -> WindowState {
        WindowState {
            id,
            width: w,
            height: h,
            is_floating: false,
            position: None,
            cooldown_until: None,
        }
    }

    #[test]
    fn test_send_tape_window_to_right() {
        let temp_dir = tempdir().unwrap();
        let win = mock_window(100, true, false, 1, None);
        let mock = MockNiri::new(vec![win]);

        let mut ctx = Ctx {
            state: AppState::default(),
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        send(&mut ctx, Target::Right).expect("send failed");

        assert_eq!(ctx.state.right.windows.len(), 1);
        assert!(ctx.state.left.windows.is_empty());

        let actions = &ctx.socket.sent_actions;
        assert!(actions.iter().any(|a| matches!(a, Action::ToggleWindowFloating { id: Some(100) })));
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowWidth { change: SizeChange::SetFixed(300), id: Some(100) }
        )));
    }

    #[test]
    fn test_send_panel_window_to_center_removes_without_readding() {
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
            cooldown_until: None,
        });

        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        send(&mut ctx, Target::Center).expect("send failed");

        assert!(ctx.state.right.windows.is_empty());
        assert!(ctx.state.left.windows.is_empty());
        assert!(ctx.state.ignored_windows.contains(&100));

        let actions = &ctx.socket.sent_actions;
        // Size restored.
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowWidth { change: SizeChange::SetFixed(1000), id: Some(100) }
        )));
    }

    #[test]
    fn test_send_across_panels() {
        let temp_dir = tempdir().unwrap();
        let win = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![win]);

        let mut config = mock_config();
        config.left.enabled = true;

        let mut state = AppState::default();
        state.right.windows.push(ws(100, 1000, 800));

        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        send(&mut ctx, Target::Left).expect("send failed");

        assert!(ctx.state.right.windows.is_empty());
        assert_eq!(ctx.state.left.windows.len(), 1);
        assert_eq!(ctx.state.left.windows[0].id, 100);
        // Cross-panel move shouldn't leave it ignored.
        assert!(!ctx.state.ignored_windows.contains(&100));
    }

    #[test]
    fn test_send_to_same_panel_is_noop_on_state() {
        let temp_dir = tempdir().unwrap();
        let win = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![win]);

        let mut state = AppState::default();
        state.right.windows.push(ws(100, 1000, 800));

        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        send(&mut ctx, Target::Right).expect("send failed");

        // State unchanged.
        assert_eq!(ctx.state.right.windows.len(), 1);
        assert_eq!(ctx.state.right.windows[0].id, 100);
        assert!(ctx.state.left.windows.is_empty());
    }

    #[test]
    fn test_send_tape_to_center_is_noop() {
        let temp_dir = tempdir().unwrap();
        let win = mock_window(100, true, false, 1, None);
        let mock = MockNiri::new(vec![win]);

        let mut ctx = Ctx {
            state: AppState::default(),
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        send(&mut ctx, Target::Center).expect("send failed");

        assert!(ctx.state.left.windows.is_empty());
        assert!(ctx.state.right.windows.is_empty());
    }

    #[test]
    fn test_send_center_un_floats_originally_floating_window() {
        // Even if the window was floating before being added to the panel,
        // `send center` returns it to the tape — semantics are "put this in
        // the tape" not "restore prior state."
        let temp_dir = tempdir().unwrap();
        let win = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![win]);

        let mut state = AppState::default();
        state.right.windows.push(WindowState {
            id: 100,
            width: 1000,
            height: 800,
            is_floating: true,
            position: Some((5.0, 6.0)),
            cooldown_until: None,
        });

        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        send(&mut ctx, Target::Center).expect("send failed");

        let actions = &ctx.socket.sent_actions;
        assert!(
            actions.iter().any(|a| matches!(a, Action::ToggleWindowFloating { id: Some(100) })),
            "send center must always un-float"
        );
    }

    #[test]
    fn test_send_panel_window_to_floating_leaves_window_untouched() {
        // send floating drops tracking but leaves the window exactly where and
        // what size it is. No size restore, no position move, no float toggle.
        let temp_dir = tempdir().unwrap();
        let win = mock_window(100, true, true, 1, Some((42.0, 77.0)));
        let mock = MockNiri::new(vec![win]);

        let mut state = AppState::default();
        state.right.windows.push(WindowState {
            id: 100,
            width: 1000,
            height: 800,
            is_floating: false,
            position: None,
            cooldown_until: None,
        });

        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        send(&mut ctx, Target::Floating).expect("send failed");

        assert!(ctx.state.right.windows.is_empty());
        assert!(ctx.state.ignored_windows.contains(&100));

        let actions = &ctx.socket.sent_actions;
        assert!(
            !actions.iter().any(|a| matches!(a, Action::ToggleWindowFloating { .. })),
            "send floating must not touch float state"
        );
        assert!(
            !actions.iter().any(|a|
                matches!(a, Action::SetWindowWidth { id: Some(100), .. })
            ),
            "send floating must not restore width — window keeps its current size"
        );
        assert!(
            !actions.iter().any(|a|
                matches!(a, Action::MoveFloatingWindow { id: Some(100), .. })
            ),
            "send floating must not move the window"
        );
    }

    #[test]
    fn test_send_tiled_untracked_to_floating_floats_it() {
        // A tape window that isn't on any panel becomes a free-floating
        // window when sent to floating — same effect as niri's own toggle.
        let temp_dir = tempdir().unwrap();
        let win = mock_window(100, true, false, 1, None);
        let mock = MockNiri::new(vec![win]);

        let mut ctx = Ctx {
            state: AppState::default(),
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        send(&mut ctx, Target::Floating).expect("send failed");

        // No tracking change — still off any panel.
        assert!(ctx.state.left.windows.is_empty());
        assert!(ctx.state.right.windows.is_empty());
        assert!(!ctx.state.ignored_windows.contains(&100));

        let actions = &ctx.socket.sent_actions;
        assert!(
            actions.iter().any(|a| matches!(a, Action::ToggleWindowFloating { id: Some(100) })),
            "send floating on a tiled tape window must float it"
        );
    }

    #[test]
    fn test_send_floating_untracked_to_floating_is_noop() {
        // Already floating and untracked — nothing to do.
        let temp_dir = tempdir().unwrap();
        let win = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![win]);

        let mut ctx = Ctx {
            state: AppState::default(),
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        send(&mut ctx, Target::Floating).expect("send failed");

        let actions = &ctx.socket.sent_actions;
        assert!(
            !actions.iter().any(|a| matches!(a, Action::ToggleWindowFloating { id: Some(100) })),
            "send floating on an already-floating window must not toggle"
        );
    }

    #[test]
    fn test_send_floating_untracked_to_center_un_floats_it() {
        // A floating window not tracked by any panel returns to the tape
        // when sent to center.
        let temp_dir = tempdir().unwrap();
        let win = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![win]);

        let mut ctx = Ctx {
            state: AppState::default(),
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        send(&mut ctx, Target::Center).expect("send failed");

        let actions = &ctx.socket.sent_actions;
        assert!(
            actions.iter().any(|a| matches!(a, Action::ToggleWindowFloating { id: Some(100) })),
            "send center on a floating untracked window must un-float it"
        );
    }

    #[test]
    fn test_send_to_disabled_panel_errors() {
        // Given: the left panel is disabled in mock_config.
        let temp_dir = tempdir().unwrap();
        let win = mock_window(100, true, false, 1, None);
        let mock = MockNiri::new(vec![win]);
        let mut ctx = Ctx {
            state: AppState::default(),
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        // When: we try to send a tape window to the disabled left panel.
        let result = send(&mut ctx, Target::Left);

        // Then: errors without state mutation or niri actions. Important —
        // before this fix the window would have been toggled to floating and
        // sized to the panel's width, then orphaned because reorder skips
        // disabled panels.
        assert!(result.is_err());
        assert!(ctx.state.left.windows.is_empty());
        assert!(ctx.state.right.windows.is_empty());
        assert!(ctx.socket.sent_actions.is_empty());
    }

    #[test]
    fn test_send_to_center_works_with_disabled_panels() {
        // Given: both panels disabled (a degenerate but valid config).
        let temp_dir = tempdir().unwrap();
        let win = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![win]);
        let mut config = mock_config();
        config.right.enabled = false;
        let mut ctx = Ctx {
            state: AppState::default(),
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        // When: we send a floating untracked window to center.
        let result = send(&mut ctx, Target::Center);

        // Then: succeeds — center doesn't target a panel, so the disabled
        // state is irrelevant. The window un-floats back to the tape.
        assert!(result.is_ok());
        let actions = &ctx.socket.sent_actions;
        assert!(actions.iter().any(|a| matches!(a, Action::ToggleWindowFloating { id: Some(100) })));
    }

    #[test]
    fn test_send_to_floating_works_with_disabled_panels() {
        // Given: both panels disabled.
        let temp_dir = tempdir().unwrap();
        let win = mock_window(100, true, false, 1, None);
        let mock = MockNiri::new(vec![win]);
        let mut config = mock_config();
        config.right.enabled = false;
        let mut ctx = Ctx {
            state: AppState::default(),
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        // When: we send a tiled untracked window to floating.
        let result = send(&mut ctx, Target::Floating);

        // Then: succeeds — floating doesn't target a panel either.
        assert!(result.is_ok());
        let actions = &ctx.socket.sent_actions;
        assert!(actions.iter().any(|a| matches!(a, Action::ToggleWindowFloating { id: Some(100) })));
    }
}
