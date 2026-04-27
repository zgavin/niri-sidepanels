use crate::config::Side;
use crate::{Ctx, NiriClient};
use anyhow::Result;
use niri_ipc::{Action, Window, WorkspaceReferenceArg};

/// Move this side's tracked windows from a given source workspace to the
/// currently active workspace.
pub fn move_from<C: NiriClient>(ctx: &mut Ctx<C>, side: Side, workspace: u64) -> Result<()> {
    ctx.config.require_enabled(side)?;
    let active_workspace = ctx.socket.get_active_workspace()?.id;
    let windows = ctx.socket.get_windows()?;
    let tracked_ids: Vec<u64> = ctx.state.panel(side).windows.iter().map(|w| w.id).collect();

    let windows_on_ws: Vec<_> = windows
        .iter()
        .filter(|w| w.workspace_id == Some(workspace) && tracked_ids.contains(&w.id))
        .collect();

    move_to(ctx, windows_on_ws, active_workspace)?;
    Ok(())
}

pub fn move_to<C: NiriClient>(ctx: &mut Ctx<C>, windows: Vec<&Window>, to_ws: u64) -> Result<()> {
    for w in windows {
        ctx.socket.send_action(Action::MoveWindowToWorkspace {
            window_id: Some(w.id),
            reference: WorkspaceReferenceArg::Id(to_ws),
            focus: false,
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppState, WindowState};
    use crate::test_utils::{MockNiri, mock_config, mock_window};
    use niri_ipc::{Action, WorkspaceReferenceArg};
    use tempfile::tempdir;

    #[test]
    fn test_move_from_only_moves_this_sides_windows() {
        let temp_dir = tempdir().unwrap();

        let mut state = AppState::default();
        // Right panel tracks id=100 and id=500.
        state.right.windows.push(WindowState {
            id: 100,
            width: 500,
            height: 500,
            is_floating: true,
            position: Some((1.0, 2.0)),
            cooldown_until: None,
        });
        state.right.windows.push(WindowState {
            id: 500,
            width: 500,
            height: 500,
            is_floating: true,
            position: Some((1.0, 2.0)),
            cooldown_until: None,
        });
        // Left panel tracks id=700.
        state.left.windows.push(WindowState {
            id: 700,
            width: 500,
            height: 500,
            is_floating: true,
            position: Some((1.0, 2.0)),
            cooldown_until: None,
        });

        let source_ws = 2;
        let target_ws = 1; // MockNiri reports active_workspace=1

        // id=100 on source, right-tracked → move
        let w100 = mock_window(100, true, false, source_ws, Some((1.0, 2.0)));
        // id=200 on source, not tracked → ignore
        let w200 = mock_window(200, true, false, source_ws, Some((1.0, 2.0)));
        // id=300 on another ws, not tracked → ignore
        let w300 = mock_window(300, true, false, 99, Some((1.0, 2.0)));
        // id=700 on source, LEFT-tracked → ignore when calling for Side::Right
        let w700 = mock_window(700, true, false, source_ws, Some((1.0, 2.0)));
        // id=400 on target, not tracked
        let w400 = mock_window(400, true, true, target_ws, Some((1.0, 2.0)));

        let mock = MockNiri::new(vec![w100, w200, w300, w700, w400]);

        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        move_from(&mut ctx, Side::Right, source_ws).expect("move_from failed");
        let actions = &ctx.socket.sent_actions;

        assert_eq!(actions.len(), 1, "only id=100 should move");

        if let Action::MoveWindowToWorkspace {
            window_id,
            reference,
            ..
        } = &actions[0]
        {
            assert_eq!(*window_id, Some(100));
            match reference {
                WorkspaceReferenceArg::Id(id) => assert_eq!(*id, target_ws),
                _ => panic!("Expected ID reference"),
            }
        } else {
            panic!("Unexpected action type");
        }
    }

    #[test]
    fn test_move_from_errors_on_disabled_side() {
        // Given: the left panel is disabled (mock_config default).
        let temp_dir = tempdir().unwrap();
        let mock = MockNiri::new(vec![]);
        let mut ctx = Ctx {
            state: AppState::default(),
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        // When: we try to move-from the disabled left panel.
        let result = move_from(&mut ctx, Side::Left, 5);

        // Then: errors before issuing any niri actions.
        assert!(result.is_err());
        assert!(ctx.socket.sent_actions.is_empty());
    }
}
