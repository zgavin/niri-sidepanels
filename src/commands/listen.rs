use crate::commands::movefrom::move_to;
use crate::commands::reorder;
use crate::commands::reorder::{LayoutCheck, check_layout, compute_layouts};
use crate::commands::togglewindow::add_to_panel;
use crate::config::{Side, load_config};
use crate::niri::connect;
use crate::state::{get_default_cache_dir, load_state, save_state};
use crate::window_rules::resolve_auto_add_side;
use crate::{Ctx, NiriClient};
use anyhow::Result;
use fslock::LockFile;
use niri_ipc::socket::Socket;
use niri_ipc::{Event, Request, Window, WindowLayout};

pub fn listen(mut ctx: Ctx<Socket>) -> Result<()> {
    let _ = ctx.socket.send(Request::EventStream)?;
    let mut read_event = ctx.socket.read_events();
    println!("niri-sidepanels: Listening for window events...");

    while let Ok(event) = read_event() {
        match event {
            Event::WindowClosed { id } => handle_close_event(id)?,
            Event::WindowFocusChanged { .. } => handle_focus_change()?,
            Event::WorkspaceActivated { id, focused: true } => handle_workspace_focus(id)?,
            Event::WindowOpenedOrChanged { window } => handle_new_window(&window)?,
            Event::WindowLayoutsChanged { changes } => handle_window_layouts_changed(changes)?,
            _ => {}
        }
    }

    Ok(())
}

fn get_ctx() -> Result<(Ctx<Socket>, LockFile)> {
    let cache_dir = get_default_cache_dir()?;
    let mut lock_path = cache_dir.clone();
    lock_path.push("instance.lock");
    let mut lock_file = LockFile::open(&lock_path)?;
    lock_file.lock()?;

    let state = load_state(&cache_dir)?;
    let config = load_config();
    let ctx = Ctx {
        state,
        config,
        socket: connect()?,
        cache_dir,
    };

    Ok((ctx, lock_file))
}

fn handle_close_event(closed_id: u64) -> Result<()> {
    let (mut ctx, _lock) = get_ctx()?;
    process_close(&mut ctx, closed_id)
}

fn handle_focus_change() -> Result<()> {
    let (mut ctx, _lock) = get_ctx()?;
    process_focus(&mut ctx)
}

fn handle_workspace_focus(ws_id: u64) -> Result<()> {
    let (mut ctx, _lock) = get_ctx()?;
    process_workspace_focus(&mut ctx, ws_id)
}

fn handle_new_window(window: &Window) -> Result<()> {
    let (mut ctx, _lock) = get_ctx()?;
    process_new_window(&mut ctx, window)
}

fn handle_window_layouts_changed(changes: Vec<(u64, WindowLayout)>) -> Result<()> {
    let (mut ctx, _lock) = get_ctx()?;
    process_window_layouts_changed(&mut ctx, &changes)
}

pub fn process_close<C: NiriClient>(ctx: &mut Ctx<C>, closed_id: u64) -> Result<()> {
    let mut dirty = false;
    for side in Side::ALL {
        let panel_state = ctx.state.panel_mut(side);
        if let Some(index) = panel_state.windows.iter().position(|w| w.id == closed_id) {
            println!("Panel {:?} window {} closed. Reordering...", side, closed_id);
            panel_state.windows.remove(index);
            dirty = true;
            break;
        }
    }
    if dirty {
        save_state(&ctx.state, &ctx.cache_dir)?;
        reorder(ctx)?;
    }
    Ok(())
}

pub fn process_focus<C: NiriClient>(ctx: &mut Ctx<C>) -> Result<()> {
    reorder(ctx)?;
    Ok(())
}

/// When the workspace changes, bring each sticky panel's windows along.
pub fn process_workspace_focus<C: NiriClient>(ctx: &mut Ctx<C>, ws_id: u64) -> Result<()> {
    let all_windows = ctx.socket.get_windows()?;
    for side in Side::ALL {
        let panel = ctx.config.panel(side);
        if !panel.sticky {
            continue;
        }
        let tracked_ids: Vec<u64> = ctx.state.panel(side).windows.iter().map(|w| w.id).collect();
        let to_move: Vec<_> = all_windows
            .iter()
            .filter(|w| tracked_ids.contains(&w.id))
            .collect();
        move_to(ctx, to_move, ws_id)?;
    }
    Ok(())
}

pub fn process_new_window<C: NiriClient>(ctx: &mut Ctx<C>, window: &Window) -> Result<()> {
    // A just-removed panel window fires a WindowOpenedOrChanged event while it
    // transitions out of the floating layout; skip it so auto_add doesn't yank
    // it straight back in.
    if let Some(index) = ctx
        .state
        .ignored_windows
        .iter()
        .position(|id| id == &window.id)
    {
        ctx.state.ignored_windows.remove(index);
        return Ok(());
    }

    if ctx.state.side_of(window.id).is_some() {
        return Ok(());
    }

    let Some(side) = resolve_auto_add_side(&ctx.config, window) else {
        return Ok(());
    };

    add_to_panel(ctx, side, window)?;
    save_state(&ctx.state, &ctx.cache_dir)?;
    reorder(ctx)?;
    Ok(())
}

/// Detect user moves/resizes of panel windows and eject them. Compares each
/// reported layout against what the daemon would have computed for that
/// window in its panel slot; drift beyond `LAYOUT_TOLERANCE_PX` means the
/// user nudged it, so we drop it from panel tracking and re-stack the rest.
pub fn process_window_layouts_changed<C: NiriClient>(
    ctx: &mut Ctx<C>,
    changes: &[(u64, WindowLayout)],
) -> Result<()> {
    // Quick path: if no changed window is currently panel-tracked, we have
    // nothing to compare against and can skip the niri queries entirely.
    let any_tracked = changes
        .iter()
        .any(|(id, _)| ctx.state.side_of(*id).is_some());
    if !any_tracked {
        return Ok(());
    }

    let screen = ctx.socket.get_screen_dimensions()?;
    let current_ws = ctx.socket.get_active_workspace()?.id;
    let all_windows = ctx.socket.get_windows()?;

    let expected = compute_layouts(&ctx.config, &ctx.state, &all_windows, current_ws, screen);

    let mut to_eject: Vec<(Side, u64)> = Vec::new();
    for (id, reported) in changes {
        let Some(side) = ctx.state.side_of(*id) else {
            continue;
        };
        let Some((_, expected_layout)) = expected.iter().find(|(eid, _)| eid == id) else {
            // Window is panel-tracked but not in the computed layout — likely
            // on a different workspace right now. Skip.
            continue;
        };
        if matches!(check_layout(expected_layout, reported), LayoutCheck::Drift) {
            println!(
                "Panel {:?} window {} drifted from expected layout. Ejecting.",
                side, id
            );
            to_eject.push((side, *id));
        }
    }

    if to_eject.is_empty() {
        return Ok(());
    }

    for (side, id) in &to_eject {
        let panel_state = ctx.state.panel_mut(*side);
        if let Some(index) = panel_state.windows.iter().position(|w| w.id == *id) {
            panel_state.windows.remove(index);
        }
        // Mark the id as ignored so the listener's auto-add path doesn't
        // immediately re-panelize the window from any side-effect events.
        ctx.state.ignored_windows.push(*id);
    }

    save_state(&ctx.state, &ctx.cache_dir)?;
    reorder(ctx)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Panel, WindowRule};
    use crate::state::{AppState, WindowState};
    use crate::test_utils::{MockNiri, mock_window};
    use niri_ipc::{Action, WorkspaceReferenceArg};
    use regex::Regex;
    use tempfile::tempdir;

    #[test]
    fn test_process_close_removes_window_from_tracked_panel() {
        let temp_dir = tempdir().unwrap();

        let mut state = AppState::default();
        state.right.windows.extend([
            WindowState {
                id: 100,
                width: 500,
                height: 500,
                is_floating: false,
                position: None,
            },
            WindowState {
                id: 200,
                width: 500,
                height: 500,
                is_floating: true,
                position: Some((1.0, 2.0)),
            },
        ]);

        let w100 = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        let w200 = mock_window(200, false, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w100, w200]);

        let mut ctx = Ctx {
            state,
            config: crate::test_utils::mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        process_close(&mut ctx, 100).expect("Process close failed");

        assert!(!ctx.state.right.windows.iter().any(|w| w.id == 100));
        assert_eq!(ctx.state.right.windows.len(), 1);
        assert_eq!(ctx.state.right.windows[0].id, 200);
        // reorder runs, so actions go out.
        assert!(!ctx.socket.sent_actions.is_empty());
    }

    #[test]
    fn test_process_close_ignores_untracked_id() {
        let temp_dir = tempdir().unwrap();
        let mut state = AppState::default();
        state.right.windows.push(WindowState {
            id: 100,
            width: 500,
            height: 500,
            is_floating: false,
            position: None,
        });
        let mock = MockNiri::new(vec![]);
        let mut ctx = Ctx {
            state,
            config: crate::test_utils::mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };
        process_close(&mut ctx, 999).expect("Process close failed");
        assert_eq!(ctx.state.right.windows.len(), 1);
        assert!(ctx.socket.sent_actions.is_empty());
    }

    #[test]
    fn test_process_workspace_focus_only_moves_sticky_panels() {
        let temp_dir = tempdir().unwrap();
        let mut state = AppState::default();
        state.left.windows.push(WindowState {
            id: 10,
            width: 100,
            height: 200,
            is_floating: true,
            position: Some((1.0, 2.0)),
        });
        state.right.windows.push(WindowState {
            id: 20,
            width: 100,
            height: 200,
            is_floating: true,
            position: Some((1.0, 2.0)),
        });

        let w10 = mock_window(10, true, false, 1, Some((1.0, 2.0)));
        let w20 = mock_window(20, true, false, 2, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w10, w20]);

        // Left sticky, right not sticky.
        let mut config = Config::default();
        config.left = Panel {
            enabled: true,
            sticky: true,
            ..Panel::default()
        };
        config.right.sticky = false;

        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        let target_ws = 99;
        process_workspace_focus(&mut ctx, target_ws).expect("process_workspace_focus failed");
        let actions = &ctx.socket.sent_actions;

        assert_eq!(actions.len(), 1, "only sticky side's window moves");
        if let Action::MoveWindowToWorkspace {
            window_id,
            reference,
            ..
        } = &actions[0]
        {
            assert_eq!(*window_id, Some(10));
            match reference {
                WorkspaceReferenceArg::Id(id) => assert_eq!(*id, target_ws),
                _ => panic!("Wrong reference type"),
            }
        } else {
            panic!("Unexpected action type");
        }
    }

    #[test]
    fn test_process_new_window_autoadds_to_right_by_default() {
        let temp_dir = tempdir().unwrap();
        let w100 = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w100]);

        let config = Config {
            window_rule: vec![WindowRule {
                app_id: Some(Regex::new(r"test").unwrap()),
                auto_add: true,
                ..Default::default()
            }],
            ..Default::default()
        };

        let mut ctx = Ctx {
            state: AppState::default(),
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        let w100 = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        process_new_window(&mut ctx, &w100).expect("Process new window failed");

        assert!(ctx.state.right.windows.iter().any(|w| w.id == 100));
    }

    #[test]
    fn test_process_new_window_autoadds_to_explicit_side() {
        let temp_dir = tempdir().unwrap();
        let w100 = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w100]);

        let mut config = Config {
            window_rule: vec![WindowRule {
                app_id: Some(Regex::new(r"test").unwrap()),
                auto_add: true,
                side: Some(Side::Left),
                ..Default::default()
            }],
            ..Default::default()
        };
        config.left.enabled = true;

        let mut ctx = Ctx {
            state: AppState::default(),
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        let w100 = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        process_new_window(&mut ctx, &w100).expect("Process new window failed");

        assert!(ctx.state.left.windows.iter().any(|w| w.id == 100));
        assert!(!ctx.state.right.windows.iter().any(|w| w.id == 100));
    }

    #[test]
    fn test_process_new_window_ignores_when_no_rule() {
        let temp_dir = tempdir().unwrap();
        let w100 = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w100]);

        let mut ctx = Ctx {
            state: AppState::default(),
            config: Config::default(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        let w100 = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        process_new_window(&mut ctx, &w100).expect("Process new window failed");

        assert_eq!(ctx.state.right.windows.len(), 0);
        assert_eq!(ctx.state.left.windows.len(), 0);
        assert!(ctx.socket.sent_actions.is_empty());
    }

    #[test]
    fn test_process_new_window_skips_ignored() {
        let temp_dir = tempdir().unwrap();
        let w100 = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w100]);

        let mut state = AppState::default();
        state.ignored_windows.push(100);

        let config = Config {
            window_rule: vec![WindowRule {
                app_id: Some(Regex::new(r"test").unwrap()),
                auto_add: true,
                ..Default::default()
            }],
            ..Default::default()
        };

        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        let w100 = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        process_new_window(&mut ctx, &w100).expect("Process new window failed");

        assert_eq!(ctx.state.right.windows.len(), 0);
        assert!(!ctx.state.ignored_windows.contains(&100));
        assert!(ctx.socket.sent_actions.is_empty());
    }

    fn reported_at(pos: Option<(f64, f64)>, size: (i32, i32)) -> WindowLayout {
        WindowLayout {
            window_size: size,
            pos_in_scrolling_layout: None,
            tile_size: (0.0, 0.0),
            tile_pos_in_workspace_view: pos,
            window_offset_in_tile: (0.0, 0.0),
        }
    }

    fn ws_state(id: u64) -> WindowState {
        WindowState {
            id,
            width: 1000,
            height: 800,
            is_floating: false,
            position: None,
        }
    }

    #[test]
    fn test_wlc_no_panel_tracked_windows_is_noop() {
        // No tracked windows means we can skip the niri queries entirely.
        let temp_dir = tempdir().unwrap();
        let mock = MockNiri::new(vec![]);
        let mut ctx = Ctx {
            state: AppState::default(),
            config: crate::test_utils::mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        let changes = vec![(42, reported_at(Some((100.0, 200.0)), (300, 400)))];
        process_window_layouts_changed(&mut ctx, &changes).expect("WLC failed");

        assert!(ctx.socket.sent_actions.is_empty());
        assert!(ctx.state.right.windows.is_empty());
    }

    #[test]
    fn test_wlc_matching_layout_does_not_eject() {
        // Window in the right panel reporting its expected layout — our own
        // echo, not a user move. No ejection, no state change.
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, Some((1600.0, 50.0)));
        let mock = MockNiri::new(vec![w1]);

        let mut state = AppState::default();
        state.right.windows.push(ws_state(1));
        let mut ctx = Ctx {
            state,
            config: crate::test_utils::mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        // 1-window layout: x=1600, y=50, height=980, width=300.
        let changes = vec![(1, reported_at(Some((1600.0, 50.0)), (300, 980)))];
        process_window_layouts_changed(&mut ctx, &changes).expect("WLC failed");

        assert_eq!(ctx.state.right.windows.len(), 1, "window must remain tracked");
        assert!(!ctx.state.ignored_windows.contains(&1));
    }

    #[test]
    fn test_wlc_drift_position_ejects() {
        // Window reports a position 200px off — clearly a user drag.
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, Some((400.0, 300.0)));
        let mock = MockNiri::new(vec![w1]);

        let mut state = AppState::default();
        state.right.windows.push(ws_state(1));
        let mut ctx = Ctx {
            state,
            config: crate::test_utils::mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        let changes = vec![(1, reported_at(Some((400.0, 300.0)), (300, 980)))];
        process_window_layouts_changed(&mut ctx, &changes).expect("WLC failed");

        assert!(ctx.state.right.windows.is_empty(), "drifted window must be ejected");
        assert!(ctx.state.ignored_windows.contains(&1));
    }

    #[test]
    fn test_wlc_drift_size_ejects() {
        // Window position is right but the user resized it — also an eject.
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, Some((1600.0, 50.0)));
        let mock = MockNiri::new(vec![w1]);

        let mut state = AppState::default();
        state.right.windows.push(ws_state(1));
        let mut ctx = Ctx {
            state,
            config: crate::test_utils::mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        // Width changed from 300 to 500 — user resized.
        let changes = vec![(1, reported_at(Some((1600.0, 50.0)), (500, 980)))];
        process_window_layouts_changed(&mut ctx, &changes).expect("WLC failed");

        assert!(ctx.state.right.windows.is_empty());
        assert!(ctx.state.ignored_windows.contains(&1));
    }

    #[test]
    fn test_wlc_drift_ejected_remaining_windows_reorder() {
        // Two tracked windows; one drifts out, the other should re-stack to fill
        // the freed space (1-window layout: height = 980).
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, Some((1600.0, 545.0)));
        let w2 = mock_window(2, false, true, 1, Some((400.0, 300.0))); // drifted
        let mock = MockNiri::new(vec![w1, w2]);

        let mut state = AppState::default();
        state.right.windows.push(ws_state(1));
        state.right.windows.push(ws_state(2));
        let mut ctx = Ctx {
            state,
            config: crate::test_utils::mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        let changes = vec![
            // id=1 at its 2-window expected slot — no drift.
            (1, reported_at(Some((1600.0, 545.0)), (300, 485))),
            // id=2 dragged off — drift.
            (2, reported_at(Some((400.0, 300.0)), (300, 485))),
        ];
        process_window_layouts_changed(&mut ctx, &changes).expect("WLC failed");

        assert_eq!(ctx.state.right.windows.len(), 1);
        assert_eq!(ctx.state.right.windows[0].id, 1);

        // Reorder fired: the surviving window should be sized to the 1-window
        // height of 980.
        let actions = &ctx.socket.sent_actions;
        assert!(
            actions.iter().any(|a| matches!(a,
                niri_ipc::Action::SetWindowHeight {
                    change: niri_ipc::SizeChange::SetFixed(980),
                    id: Some(1)
                }
            )),
            "remaining window must re-stack to fill the panel"
        );
    }

    #[test]
    fn test_wlc_unknown_window_ignored() {
        // WLC reports a window niri-sidepanels has never tracked. No-op even
        // if the layout would mismatch what we'd compute.
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, Some((1600.0, 50.0)));
        let mock = MockNiri::new(vec![w1]);

        let mut state = AppState::default();
        state.right.windows.push(ws_state(1));
        let mut ctx = Ctx {
            state,
            config: crate::test_utils::mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        // Unknown id 999 with arbitrary layout — should not eject anything.
        let changes = vec![(999, reported_at(Some((42.0, 42.0)), (1, 1)))];
        process_window_layouts_changed(&mut ctx, &changes).expect("WLC failed");

        assert_eq!(ctx.state.right.windows.len(), 1);
        assert!(!ctx.state.ignored_windows.contains(&999));
    }
}
