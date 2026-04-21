use crate::config::Side;
use crate::niri::NiriClient;
use crate::state::WindowState;
use crate::{Ctx, Direction};
use anyhow::Result;
use niri_ipc::Action;

pub fn focus<C: NiriClient>(ctx: &mut Ctx<C>, side: Side, direction: Direction) -> Result<()> {
    let windows = &ctx.state.panel(side).windows;
    let len = windows.len();
    if len == 0 {
        return Ok(());
    }

    let active_window = ctx.socket.get_active_window()?.id;
    let current_index_opt = windows.iter().position(|w| w.id == active_window);

    let next_index = if let Some(i) = current_index_opt {
        match direction {
            Direction::Next => (i + len - 1) % len,
            Direction::Prev => (i + 1) % len,
        }
    } else {
        match direction {
            Direction::Next => len - 1,
            Direction::Prev => 0,
        }
    };

    if let Some(WindowState { id, .. }) = windows.get(next_index) {
        let _ = ctx.socket.send_action(Action::FocusWindow { id: *id });
    }

    Ok(())
}

#[cfg(test)]
mod tests_focus {
    use super::*;
    use crate::Direction;
    use crate::state::{AppState, WindowState};
    use crate::test_utils::{MockNiri, mock_config, mock_window};
    use niri_ipc::Action;
    use tempfile::tempdir;

    fn ws(id: u64) -> WindowState {
        WindowState {
            id,
            width: 100,
            height: 100,
            is_floating: true,
            position: None,
        }
    }

    #[test]
    fn test_cycle_focus_next_within_side() {
        let temp_dir = tempdir().unwrap();
        // Right panel [1, 2, 3]; focused is 2. Next => wrap to 1.
        let win_a = mock_window(1, false, true, 1, Some((1.0, 2.0)));
        let win_b = mock_window(2, true, true, 1, Some((1.0, 2.0)));
        let win_c = mock_window(3, false, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![win_a, win_b, win_c]);

        let mut state = AppState::default();
        state.right.windows.extend([ws(1), ws(2), ws(3)]);

        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        focus(&mut ctx, Side::Right, Direction::Next).unwrap();
        assert!(ctx.socket.sent_actions.iter().any(|a| matches!(a, Action::FocusWindow { id: 1 })));
    }

    #[test]
    fn test_focus_isolated_per_side() {
        let temp_dir = tempdir().unwrap();
        // Left has [10]; right has [1, 2, 3]. focus(left, Next) cycles within left only.
        let w10 = mock_window(10, true, true, 1, None);
        let w1 = mock_window(1, false, true, 1, None);
        let w2 = mock_window(2, false, true, 1, None);
        let w3 = mock_window(3, false, true, 1, None);
        let mock = MockNiri::new(vec![w10, w1, w2, w3]);

        let mut state = AppState::default();
        state.left.windows.push(ws(10));
        state.right.windows.extend([ws(1), ws(2), ws(3)]);

        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        focus(&mut ctx, Side::Left, Direction::Next).unwrap();
        // Only one window on left; should focus it (10).
        assert!(ctx.socket.sent_actions.iter().any(|a| matches!(a, Action::FocusWindow { id: 10 })));
        // Should not touch right-panel ids.
        assert!(!ctx.socket.sent_actions.iter().any(|a| matches!(a, Action::FocusWindow { id: 1 | 2 | 3 })));
    }

    #[test]
    fn test_focus_from_outside_enters_panel() {
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, None);
        let w2 = mock_window(2, false, true, 1, None);
        let w_outside = mock_window(99, true, false, 1, None);
        let mock = MockNiri::new(vec![w1, w2, w_outside]);

        let mut state = AppState::default();
        state.right.windows.extend([ws(1), ws(2)]);

        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        // Next when active is outside → focus last entry on that side (id=2).
        focus(&mut ctx, Side::Right, Direction::Next).unwrap();
        assert!(ctx.socket.sent_actions.iter().any(|a| matches!(a, Action::FocusWindow { id: 2 })));

        ctx.socket.sent_actions.clear();
        // Prev when active is outside → focus first (id=1).
        focus(&mut ctx, Side::Right, Direction::Prev).unwrap();
        assert!(ctx.socket.sent_actions.iter().any(|a| matches!(a, Action::FocusWindow { id: 1 })));
    }

    #[test]
    fn test_focus_empty_panel_noop() {
        let temp_dir = tempdir().unwrap();
        let win = mock_window(99, true, false, 1, None);
        let mock = MockNiri::new(vec![win]);

        let mut ctx = Ctx {
            state: AppState::default(),
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        focus(&mut ctx, Side::Right, Direction::Next).unwrap();
        assert!(ctx.socket.sent_actions.is_empty());
    }
}
