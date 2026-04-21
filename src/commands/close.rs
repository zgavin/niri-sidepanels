use crate::Ctx;
use crate::commands::reorder;
use crate::niri::NiriClient;
use crate::state::save_state;
use anyhow::{Context, Result};
use niri_ipc::Action;

/// Close the focused window. If it's currently tracked on either panel,
/// drop it from that panel's state. Side is inferred from state.
pub fn close<C: NiriClient>(ctx: &mut Ctx<C>) -> Result<()> {
    let windows = ctx.socket.get_windows()?;
    let focused = windows
        .iter()
        .find(|w| w.is_focused)
        .context("No window focused")?;

    let mut dirty = false;
    for side in crate::config::Side::ALL {
        let panel_state = ctx.state.panel_mut(side);
        if let Some(index) = panel_state.windows.iter().position(|w| w.id == focused.id) {
            panel_state.windows.remove(index);
            dirty = true;
            break;
        }
    }
    if dirty {
        save_state(&ctx.state, &ctx.cache_dir)?;
    }

    let _ = ctx.socket.send_action(Action::CloseWindow {
        id: Some(focused.id),
    });
    reorder(ctx)?;

    Ok(())
}

#[cfg(test)]
mod tests_close {
    use super::*;
    use crate::config::Side;
    use crate::state::{AppState, WindowState};
    use crate::test_utils::{MockNiri, mock_config, mock_window};
    use niri_ipc::Action;
    use tempfile::tempdir;

    #[test]
    fn test_close_panel_window_removes_from_correct_side() {
        let temp_dir = tempdir().unwrap();
        let win = mock_window(10, true, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![win]);

        let mut state = AppState::default();
        state.right.windows.push(WindowState {
            id: 10,
            width: 100,
            height: 100,
            is_floating: false,
            position: None,
        });

        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        close(&mut ctx).expect("Close failed");

        assert!(ctx.state.right.windows.is_empty());
        assert!(ctx.state.left.windows.is_empty());

        assert!(ctx.socket.sent_actions.iter().any(|a| matches!(a, Action::CloseWindow { id: Some(10) })));
    }

    #[test]
    fn test_close_left_panel_window_removes_from_left_only() {
        let temp_dir = tempdir().unwrap();
        let win = mock_window(10, true, true, 1, Some((1.0, 2.0)));
        let other = mock_window(20, false, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![win, other]);

        let mut state = AppState::default();
        state.left.windows.push(WindowState {
            id: 10,
            width: 100,
            height: 100,
            is_floating: false,
            position: None,
        });
        state.right.windows.push(WindowState {
            id: 20,
            width: 100,
            height: 100,
            is_floating: false,
            position: None,
        });

        let mut ctx = Ctx {
            state,
            config: {
                let mut c = mock_config();
                c.left.enabled = true;
                c
            },
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        close(&mut ctx).expect("Close failed");

        assert!(ctx.state.left.windows.is_empty());
        assert_eq!(ctx.state.right.windows.len(), 1);
        assert_eq!(ctx.state.right.windows[0].id, 20);
        // silence unused warning in stable Side import
        let _ = Side::Left;
    }

    #[test]
    fn test_close_untracked_window_still_sends_close_action() {
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(99, true, false, 1, None);
        let w2 = mock_window(10, false, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w1, w2]);

        let mut state = AppState::default();
        state.right.windows.push(WindowState {
            id: 10,
            width: 100,
            height: 100,
            is_floating: false,
            position: None,
        });

        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        close(&mut ctx).expect("Close failed");

        assert_eq!(ctx.state.right.windows.len(), 1);
        assert!(ctx.socket.sent_actions.iter().any(|a| matches!(a, Action::CloseWindow { id: Some(99) })));
    }
}
