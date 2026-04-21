use crate::Ctx;
use crate::commands::reorder;
use crate::config::Side;
use crate::niri::NiriClient;
use crate::state::save_state;
use anyhow::Result;

pub fn toggle_visibility<C: NiriClient>(ctx: &mut Ctx<C>, side: Side) -> Result<()> {
    let panel_state = ctx.state.panel_mut(side);
    panel_state.is_hidden = !panel_state.is_hidden;
    save_state(&ctx.state, &ctx.cache_dir)?;
    reorder(ctx)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppState, PanelState, WindowState};
    use crate::test_utils::{MockNiri, mock_config, mock_window};
    use niri_ipc::{Action, PositionChange};
    use tempfile::tempdir;

    #[test]
    fn test_toggle_visibility_right() {
        let temp_dir = tempdir().unwrap();
        let win = mock_window(100, false, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![win]);

        let state = AppState {
            right: PanelState {
                windows: vec![WindowState {
                    id: 100,
                    width: 300,
                    height: 500,
                    is_floating: true,
                    position: None,
                }],
                is_hidden: false,
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

        toggle_visibility(&mut ctx, Side::Right).expect("Toggle failed");
        assert!(ctx.state.right.is_hidden);

        // Screen 1920, peek 10 → hidden_x = 1910.
        let actions = &ctx.socket.sent_actions;
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::MoveFloatingWindow {
                id: Some(100),
                x: PositionChange::SetFixed(1910.0),
                ..
            }
        )));

        ctx.socket.sent_actions.clear();
        toggle_visibility(&mut ctx, Side::Right).expect("Toggle failed");
        assert!(!ctx.state.right.is_hidden);

        // Visible X = 1920 - 300 - 20 = 1600.
        let actions = &ctx.socket.sent_actions;
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::MoveFloatingWindow {
                id: Some(100),
                x: PositionChange::SetFixed(1600.0),
                ..
            }
        )));
    }
}
