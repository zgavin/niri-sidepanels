use crate::Ctx;
use crate::commands::reorder;
use crate::config::Side;
use crate::niri::NiriClient;
use crate::state::save_state;
use anyhow::Result;

pub fn toggle_flip<C: NiriClient>(ctx: &mut Ctx<C>, side: Side) -> Result<()> {
    let panel_state = ctx.state.panel_mut(side);
    panel_state.is_flipped = !panel_state.is_flipped;
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
    fn test_toggle_flip_right() {
        let temp_dir = tempdir().unwrap();

        let w1 = mock_window(1, false, true, 1, None);
        let w2 = mock_window(2, true, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w1, w2]);

        let state = AppState {
            right: PanelState {
                windows: vec![
                    WindowState {
                        id: 1,
                        width: 300,
                        height: 200,
                        is_floating: true,
                        position: None,
                    },
                    WindowState {
                        id: 2,
                        width: 300,
                        height: 200,
                        is_floating: true,
                        position: Some((1.0, 2.0)),
                    },
                ],
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

        toggle_flip(&mut ctx, Side::Right).expect("Toggle flip failed");
        assert!(ctx.state.right.is_flipped);
        assert!(!ctx.state.left.is_flipped);

        // After flip: id=2 at base_y 830.
        let actions = &ctx.socket.sent_actions;
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::MoveFloatingWindow {
                id: Some(2),
                y: PositionChange::SetFixed(830.0),
                ..
            }
        )));
    }

    #[test]
    fn test_toggle_flip_left_only_affects_left() {
        let temp_dir = tempdir().unwrap();
        let mock = MockNiri::new(vec![]);

        let mut ctx = Ctx {
            state: AppState::default(),
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        toggle_flip(&mut ctx, Side::Left).expect("Toggle flip failed");
        assert!(ctx.state.left.is_flipped);
        assert!(!ctx.state.right.is_flipped);
    }
}
