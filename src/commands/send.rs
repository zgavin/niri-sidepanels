use crate::Ctx;
use crate::commands::reorder;
use crate::commands::togglewindow::{add_to_panel, remove_from_panel};
use crate::config::Side;
use crate::niri::NiriClient;
use crate::state::save_state;
use anyhow::Result;
use clap::ValueEnum;

/// Where to send the focused window.
///
/// `Left` / `Right` place the window on that panel (moving it across if it's
/// currently on the other panel). `Center` removes it from whichever panel is
/// tracking it and returns it to the regular niri tape.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum Target {
    Left,
    Right,
    Center,
}

impl Target {
    fn as_side(self) -> Option<Side> {
        match self {
            Target::Left => Some(Side::Left),
            Target::Right => Some(Side::Right),
            Target::Center => None,
        }
    }
}

pub fn send<C: NiriClient>(ctx: &mut Ctx<C>, target: Target) -> Result<()> {
    let focused = ctx.socket.get_active_window()?;
    let current = ctx.state.side_of(focused.id);

    match (current, target.as_side()) {
        (Some(current_side), Some(target_side)) if current_side == target_side => {
            // Already on target panel — no state change.
        }
        (Some(current_side), Some(target_side)) => {
            // Panel → other panel.
            remove_from_panel(ctx, current_side, &focused)?;
            add_to_panel(ctx, target_side, &focused)?;
            // remove_from_panel pushed the id into ignored_windows; undo that
            // so the listener doesn't skip the add's follow-up events.
            ctx.state.ignored_windows.retain(|id| *id != focused.id);
        }
        (Some(current_side), None) => {
            // Panel → center: just remove.
            remove_from_panel(ctx, current_side, &focused)?;
        }
        (None, Some(target_side)) => {
            // Tape → panel: add.
            add_to_panel(ctx, target_side, &focused)?;
        }
        (None, None) => {
            // Already on the tape — no-op.
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
}
