use crate::config::{Config, Panel, Side, WindowRule};
use crate::niri::NiriClient;
use crate::state::{AppState, PanelState, save_state};
use crate::window_rules::{resolve_rule_focus_peek, resolve_rule_peek, resolve_window_size};
use crate::{Ctx, ExpectedLayout, WindowTarget};
use anyhow::Result;
use niri_ipc::{Action, PositionChange, SizeChange, Window, WindowLayout};
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

/// Wall-clock millis since the Unix epoch. Used to set / compare per-window
/// cooldown deadlines for the eject-on-drag logic. Falls back to 0 if the
/// system clock is somehow before 1970, which would only happen if the host
/// clock is wildly broken — in which case cooldowns will never trigger,
/// which degrades gracefully back to PR #6's behavior.
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Never shrink a panel window below this height even if many windows are
/// stacked. Prevents division producing unusable or negative heights.
const MIN_WINDOW_HEIGHT: i32 = 80;

/// Drift threshold for treating a window's reported layout as a user move or
/// resize rather than our own echo. Sub-pixel noise stays below this; any
/// real drag (~several pixels minimum) clears it.
pub(crate) const LAYOUT_TOLERANCE_PX: f64 = 1.0;

/// Result of comparing a niri-reported layout against the daemon's expected
/// layout for a panel window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayoutCheck {
    /// Reported layout matches expected within tolerance — likely our own
    /// echo from a recent reorder.
    Match,
    /// Reported layout differs from expected — the user moved or resized.
    Drift,
    /// Reported layout doesn't carry enough information to decide (e.g.
    /// `tile_pos_in_workspace_view` is `None`). Treat as match-by-default
    /// rather than spuriously eject.
    Insufficient,
}

/// Compare a niri-reported `WindowLayout` against a daemon-computed
/// `ExpectedLayout`. Drift is **position-only** — size differences are
/// intentionally ignored, for two reasons:
///
/// 1. Apps with a hard minimum size (VS Code, etc.) refuse niri's
///    `SetWindowWidth` requests when our panel width is below their
///    minimum. niri reports the app's actual size, which differs from
///    our expected. Treating that as drift would eject the window on
///    every reorder pass — the panel would be unusable for any
///    min-size app.
/// 2. niri's interactive resize emits WLC per-frame (unlike interactive
///    move). Treating size drift as drift would eject the window on the
///    first frame, interrupting the user mid-action.
///
/// Position-drift catches the user-drag case, which is the primary
/// "I want this out of the panel" affordance. Resize-as-eject is left
/// out deliberately — see `v3_dynamic_panels.md` for the long-term
/// vision where resize reshapes the panel rather than ejecting from it.
pub(crate) fn check_layout(expected: &ExpectedLayout, reported: &WindowLayout) -> LayoutCheck {
    let Some((rx, ry)) = reported.tile_pos_in_workspace_view else {
        return LayoutCheck::Insufficient;
    };
    let pos_drift = (rx - expected.x as f64).abs() >= LAYOUT_TOLERANCE_PX
        || (ry - expected.y as f64).abs() >= LAYOUT_TOLERANCE_PX;
    if pos_drift {
        LayoutCheck::Drift
    } else {
        LayoutCheck::Match
    }
}

fn resolve_width(window: &Window, panel: &Panel, rules: &[WindowRule]) -> i32 {
    let (width, _) = resolve_window_size(rules, window, panel.width, panel.height);
    width
}

fn calculate_coordinates(
    side: Side,
    panel: &Panel,
    state: &PanelState,
    dims: WindowTarget,
    screen: (i32, i32),
    y: i32,
    active_peek: i32,
) -> (i32, i32) {
    let (sw, _) = screen;
    let w = dims.width;
    let margins = &panel.margins;

    match side {
        Side::Right => {
            let visible_x = sw - w - margins.right;
            let hidden_x = sw - active_peek;
            let x = if state.is_hidden { hidden_x } else { visible_x };
            (x, y)
        }
        Side::Left => {
            let visible_x = margins.left;
            let hidden_x = -w + active_peek;
            let x = if state.is_hidden { hidden_x } else { visible_x };
            (x, y)
        }
    }
}

/// Divide the panel's available vertical space among `n` windows with `gap`
/// pixels between them. Returns the per-window height, clamped to
/// [MIN_WINDOW_HEIGHT, available_height].
fn equal_height(screen_h: i32, margins_top: i32, margins_bottom: i32, gap: i32, n: usize) -> i32 {
    if n == 0 {
        return 0;
    }
    let available = screen_h - margins_top - margins_bottom;
    let total_gap = gap * (n as i32 - 1);
    let per = (available - total_gap) / n as i32;
    per.max(MIN_WINDOW_HEIGHT)
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

    // Drift-eject pass: any panel-tracked window whose niri-reported layout
    // diverges from what we'd compute is treated as a user-moved window and
    // ejected from panel tracking — *unless* it's still in its post-reorder
    // cooldown, in which case the divergence is our own animation in flight.
    //
    // niri does not emit `WindowLayoutsChanged` for interactive (mouse)
    // floating drags (its IPC layout uses settled `logical_pos`, skipping
    // the animated render offset to avoid IPC spam). So the WLC handler
    // alone misses drag-driven drifts and we have to do this check on every
    // reorder pass to catch them.
    let now = now_ms();
    for side in Side::ALL {
        let mut to_eject: Vec<u64> = Vec::new();
        let mut layouts = Vec::new();
        compute_layout_for_side(
            &ctx.config,
            &ctx.state,
            side,
            &all_windows,
            current_ws,
            (display_w, display_h),
            &mut layouts,
        );
        for (id, _expected) in &layouts {
            let Some(window) = all_windows.iter().find(|w| w.id == *id) else {
                continue;
            };
            let w_state = ctx
                .state
                .panel(side)
                .windows
                .iter()
                .find(|w| w.id == *id);
            let cooldown = w_state.and_then(|w| w.cooldown_until);
            let last_applied = w_state.and_then(|w| w.last_applied);
            // Skip drift-eject when we have no `last_applied` baseline to
            // compare against (window not yet positioned by us) or when the
            // window is still settling from a recent reorder pass.
            //
            // Comparing against `last_applied` rather than the freshly
            // computed expected matters for the case where a sibling was
            // just ejected: the survivors' freshly-recomputed expected
            // changes (panel re-divides), but their actual reported layout
            // still matches what we last applied — which means *no user
            // drift has happened*, just the layout-recomputation lag.
            let Some(baseline) = last_applied else {
                continue;
            };
            let is_settling = cooldown.is_some_and(|d| d > now);
            if is_settling {
                continue;
            }
            if matches!(check_layout(&baseline, &window.layout), LayoutCheck::Drift) {
                println!(
                    "Panel {:?} window {} drifted from expected layout. Ejecting.",
                    side, id
                );
                to_eject.push(*id);
            }
        }
        if !to_eject.is_empty() {
            let panel_state = ctx.state.panel_mut(side);
            panel_state.windows.retain(|w| !to_eject.contains(&w.id));
            for id in to_eject {
                ctx.state.ignored_windows.push(id);
            }
            dirty = true;
        }
    }
    if dirty {
        save_state(&ctx.state, &ctx.cache_dir)?;
    }

    for side in Side::ALL {
        reorder_side(ctx, side, &all_windows, current_ws, (display_w, display_h))?;
    }

    // reorder_side may have set per-window cooldown timestamps; persist them
    // so the WLC handler in the daemon process sees the same values that the
    // user-CLI invocation just wrote.
    save_state(&ctx.state, &ctx.cache_dir)?;
    Ok(())
}

/// Compute the expected layout for every panel-tracked window currently on
/// the active workspace. Pure function — same inputs, same outputs, no I/O.
/// Used both by `reorder` (to drive niri actions) and by the
/// `WindowLayoutsChanged` listener (to compare against reported layouts).
pub(crate) fn compute_layouts(
    config: &Config,
    state: &AppState,
    all_windows: &[Window],
    current_ws: u64,
    screen: (i32, i32),
) -> Vec<(u64, ExpectedLayout)> {
    let mut out = Vec::new();
    for side in Side::ALL {
        compute_layout_for_side(config, state, side, all_windows, current_ws, screen, &mut out);
    }
    out
}

fn compute_layout_for_side(
    config: &Config,
    state: &AppState,
    side: Side,
    all_windows: &[Window],
    current_ws: u64,
    screen: (i32, i32),
    out: &mut Vec<(u64, ExpectedLayout)>,
) {
    let panel = config.panel(side);
    if !panel.enabled {
        return;
    }

    let panel_state = state.panel(side);
    let ids: Vec<u64> = panel_state.windows.iter().map(|w| w.id).collect();

    let mut windows: Vec<_> = all_windows
        .iter()
        .filter(|w| w.is_floating && w.workspace_id == Some(current_ws) && ids.contains(&w.id))
        .collect();

    windows.sort_by_key(|w| ids.iter().position(|id| *id == w.id).unwrap_or(usize::MAX));
    if panel_state.is_flipped {
        windows.reverse();
    }

    let n = windows.len();
    if n == 0 {
        return;
    }

    let (_, screen_h) = screen;
    // niri's `MoveFloatingWindow` operates in working-area coordinates: it
    // adds `working_area_loc.y` to whatever y we send. So we shrink the
    // effective height by the user's configured bar zones rather than offset
    // — niri does the offset for us.
    let working_h = (screen_h - config.bars.top - config.bars.bottom).max(0);
    let gap = panel.gap;
    let per_height = equal_height(working_h, panel.margins.top, panel.margins.bottom, gap, n);

    // Layout bottom-up: first window at the bottom, subsequent windows stacked
    // above with `gap` between them. Coordinates are in working-area space.
    for (i, window) in windows.iter().enumerate() {
        let width = resolve_width(window, panel, &config.window_rule);
        let dims = WindowTarget { width, height: per_height };

        let active_peek = if window.is_focused {
            resolve_rule_focus_peek(&config.window_rule, window, panel.get_focus_peek())
        } else {
            resolve_rule_peek(&config.window_rule, window, panel.peek)
        };

        let stack_y = working_h
            - panel.margins.bottom
            - per_height
            - (i as i32) * (per_height + gap);

        let (target_x, target_y) = calculate_coordinates(
            side, panel, panel_state, dims, screen, stack_y, active_peek,
        );

        // Translate to output coords (what niri reports back via
        // `tile_pos_in_workspace_view`) by adding the bar offsets. niri's
        // `move_window` adds `working_area_loc` to whatever we send, so for a
        // round-trip-equal `ExpectedLayout`/reported comparison we need to
        // store the post-translation values here. `apply_layouts` reverses
        // the translation before sending.
        out.push((
            window.id,
            ExpectedLayout {
                x: target_x + config.bars.left,
                y: target_y + config.bars.top,
                width,
                height: per_height,
            },
        ));
    }
}

fn apply_layouts<C: NiriClient>(
    socket: &mut C,
    layouts: &[(u64, ExpectedLayout)],
    bars: &crate::config::Bars,
) {
    for (id, layout) in layouts {
        // niri ignores redundant size requests so this is cheap on unchanged
        // layouts.
        let _ = socket.send_action(Action::SetWindowWidth {
            change: SizeChange::SetFixed(layout.width),
            id: Some(*id),
        });
        let _ = socket.send_action(Action::SetWindowHeight {
            change: SizeChange::SetFixed(layout.height),
            id: Some(*id),
        });
        // ExpectedLayout is in output coords (so it can be compared apples-to-
        // apples with niri's `tile_pos_in_workspace_view` reports). niri's
        // `MoveFloatingWindow` takes working-area coords and adds
        // `working_area_loc` itself, so we subtract our bars offsets here to
        // round-trip cleanly.
        let _ = socket.send_action(Action::MoveFloatingWindow {
            id: Some(*id),
            x: PositionChange::SetFixed((layout.x - bars.left).into()),
            y: PositionChange::SetFixed((layout.y - bars.top).into()),
        });
    }
}

fn reorder_side<C: NiriClient>(
    ctx: &mut Ctx<C>,
    side: Side,
    all_windows: &[Window],
    current_ws: u64,
    screen: (i32, i32),
) -> Result<()> {
    let mut layouts = Vec::new();
    compute_layout_for_side(
        &ctx.config,
        &ctx.state,
        side,
        all_windows,
        current_ws,
        screen,
        &mut layouts,
    );
    apply_layouts(&mut ctx.socket, &layouts, &ctx.config.bars);

    // Mark each affected window as "still settling" so the WLC handler can
    // distinguish niri's animation frames from real user moves. *Only* mark
    // windows whose layout actually changed — niri ignores no-op move/size
    // actions and won't emit WLC events for them, so a blanket cooldown
    // would wrongly suppress user drags after every focus change (which
    // fires reorder but rarely changes any actual position).
    if !layouts.is_empty() {
        let cooldown_end = now_ms() + ctx.config.animation.cooldown_ms;
        let panel_state = ctx.state.panel_mut(side);
        for (id, expected) in &layouts {
            let Some(window) = all_windows.iter().find(|w| w.id == *id) else {
                continue;
            };
            // If niri's current layout already matches what we'd compute,
            // no animation will run — skip the cooldown for this window.
            let verdict = check_layout(expected, &window.layout);
            eprintln!(
                "[reorder] side={side:?} id={id}: expected={expected:?} \
                 reported_pos={:?} reported_size={:?} verdict={verdict:?}",
                window.layout.tile_pos_in_workspace_view, window.layout.window_size
            );
            if matches!(verdict, LayoutCheck::Match) {
                eprintln!("[reorder] side={side:?} id={id}: layout unchanged, no cooldown");
                continue;
            }
            if let Some(w) = panel_state.windows.iter_mut().find(|w| w.id == *id) {
                w.cooldown_until = Some(cooldown_end);
                w.last_applied = Some(*expected);
                eprintln!(
                    "[reorder] side={side:?} id={id}: cooldown set until {cooldown_end} \
                     ({}ms from now), last_applied={expected:?}",
                    ctx.config.animation.cooldown_ms
                );
            }
        }
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
            cooldown_until: None,
            last_applied: None,
        }
    }

    // mock_config: screen 1920x1080, right panel width=300 gap=10 margins
    // top=50 right=20 left=10 bottom=50. Usable vertical = 1080-50-50 = 980.

    fn find_move_y(actions: &[Action], id: u64) -> Option<f64> {
        actions.iter().find_map(|a| match a {
            Action::MoveFloatingWindow {
                id: Some(aid),
                y: PositionChange::SetFixed(y),
                ..
            } if *aid == id => Some(*y),
            _ => None,
        })
    }

    #[test]
    fn test_equal_height_one_window_fills_available() {
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, None);
        let mock = MockNiri::new(vec![w1]);

        let state = AppState {
            right: panel_state_with(vec![win_state(1)]),
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
        // N=1 → per_height = 980 (entire usable vertical)
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowHeight { change: SizeChange::SetFixed(980), id: Some(1) }
        )));
        // stack_y = 1080 - 50 - 980 - 0 = 50
        assert_eq!(find_move_y(actions, 1), Some(50.0));
    }

    #[test]
    fn test_equal_height_two_windows_divide_equally() {
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, None);
        let w2 = mock_window(2, false, true, 1, None);
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
        // N=2: per_height = (980 - 10) / 2 = 485
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowHeight { change: SizeChange::SetFixed(485), id: Some(1) }
        )));
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowHeight { change: SizeChange::SetFixed(485), id: Some(2) }
        )));
        // Bottom window (id=1): stack_y = 1080 - 50 - 485 = 545
        // Top window (id=2): stack_y = 545 - 10 - 485 = 50
        assert_eq!(find_move_y(actions, 1), Some(545.0));
        assert_eq!(find_move_y(actions, 2), Some(50.0));
    }

    #[test]
    fn test_equal_height_many_windows_clamped_to_min() {
        let temp_dir = tempdir().unwrap();
        // 20 windows with gap 10 would normally give tiny heights; clamp to min.
        let mut windows = vec![];
        let mut state_windows = vec![];
        for i in 1..=20 {
            windows.push(mock_window(i, false, true, 1, None));
            state_windows.push(win_state(i));
        }
        let mock = MockNiri::new(windows);

        let state = AppState {
            right: panel_state_with(state_windows),
            ..Default::default()
        };

        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).expect("Reorder failed");
        // (980 - 19*10) / 20 = 790/20 = 39.5 → 39, then clamped to MIN_WINDOW_HEIGHT = 80
        let actions = &ctx.socket.sent_actions;
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowHeight { change: SizeChange::SetFixed(80), id: Some(1) }
        )));
    }

    #[test]
    fn test_right_hidden_with_focus_peek_still_respects_equal_height() {
        let temp_dir = tempdir().unwrap();
        let w_focused = mock_window(1, true, true, 1, None);
        let w_bg = mock_window(2, false, true, 1, None);
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
        // Unfocused (id=2): x = 1920 - peek(10) = 1910
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
    fn test_gc_retains_only_active_windows() {
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, None);
        let w2 = mock_window(2, false, true, 99, None); // wrong workspace
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

        // Zombie (id=3) dropped, id=1 and id=2 kept in state.
        let ids: Vec<u64> = ctx.state.right.windows.iter().map(|w| w.id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        assert!(!ids.contains(&3));

        // Only id=1 actually gets reordered (id=2 is on a different ws).
        let actions = &ctx.socket.sent_actions;
        assert!(actions.iter().any(|a| matches!(a, Action::MoveFloatingWindow { id: Some(1), .. })));
        assert!(!actions.iter().any(|a| matches!(a, Action::MoveFloatingWindow { id: Some(2), .. })));
    }

    #[test]
    fn test_flipped_order_inverts_stack() {
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, None);
        let w2 = mock_window(2, false, true, 1, None);
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

        // With flip, id=2 is at the bottom slot, id=1 above.
        // per_height = 485, bottom_y = 545, top_y = 50
        assert_eq!(find_move_y(&ctx.socket.sent_actions, 2), Some(545.0));
        assert_eq!(find_move_y(&ctx.socket.sent_actions, 1), Some(50.0));
    }

    #[test]
    fn test_left_panel_hidden_geometry_unchanged() {
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, None);
        let mock = MockNiri::new(vec![w1]);

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
        // width=300, peek=10 → x = -300 + 10 = -290
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow { id: Some(1), x: PositionChange::SetFixed(x), .. }
            if *x == -290.0
        )));
    }

    #[test]
    fn test_both_panels_independent_heights() {
        let temp_dir = tempdir().unwrap();
        // Left has 1 window (height=980), right has 2 (height=485 each).
        let w_left = mock_window(10, false, true, 1, None);
        let w_r1 = mock_window(20, false, true, 1, None);
        let w_r2 = mock_window(21, false, true, 1, None);
        let mock = MockNiri::new(vec![w_left, w_r1, w_r2]);

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
            right: panel_state_with(vec![win_state(20), win_state(21)]),
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

        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowHeight { change: SizeChange::SetFixed(980), id: Some(10) }
        )));
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowHeight { change: SizeChange::SetFixed(485), id: Some(20) }
        )));
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowHeight { change: SizeChange::SetFixed(485), id: Some(21) }
        )));
    }

    #[test]
    fn test_window_rules_still_override_width_not_height() {
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, None);
        let mut w2 = mock_window(2, false, true, 1, None);
        w2.app_id = Some("wide".into());
        let mock = MockNiri::new(vec![w1, w2]);

        let mut config = mock_config();
        config.window_rule = vec![WindowRule {
            app_id: Some(Regex::new("wide").unwrap()),
            width: Some(500),
            height: Some(600), // should be ignored — equal-height overrides
            ..Default::default()
        }];

        let state = AppState {
            right: panel_state_with(vec![win_state(1), win_state(2)]),
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

        // id=2: width=500 (rule), height=485 (equal-height, NOT 600 from rule).
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowWidth { change: SizeChange::SetFixed(500), id: Some(2) }
        )));
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowHeight { change: SizeChange::SetFixed(485), id: Some(2) }
        )));
        // id=1 uses panel default width=300.
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowWidth { change: SizeChange::SetFixed(300), id: Some(1) }
        )));
    }

    #[test]
    fn test_configurable_gap_affects_stacking() {
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, None);
        let w2 = mock_window(2, false, true, 1, None);
        let mock = MockNiri::new(vec![w1, w2]);

        let mut config = mock_config();
        config.right.gap = 50; // bigger gap

        let state = AppState {
            right: panel_state_with(vec![win_state(1), win_state(2)]),
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
        // N=2, gap=50: per_height = (980 - 50) / 2 = 465
        assert!(actions.iter().any(|a| matches!(a,
            Action::SetWindowHeight { change: SizeChange::SetFixed(465), id: Some(1) }
        )));
        // bottom: 1080 - 50 - 465 = 565
        // top: 565 - 50 - 465 = 50
        assert_eq!(find_move_y(actions, 1), Some(565.0));
        assert_eq!(find_move_y(actions, 2), Some(50.0));
    }

    #[test]
    fn test_disabled_panel_is_skipped() {
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, None);
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

    fn reported_at(pos: Option<(f64, f64)>, size: (i32, i32)) -> WindowLayout {
        WindowLayout {
            window_size: size,
            pos_in_scrolling_layout: None,
            tile_size: (0.0, 0.0),
            tile_pos_in_workspace_view: pos,
            window_offset_in_tile: (0.0, 0.0),
        }
    }

    #[test]
    fn test_check_layout_exact_match() {
        // Given: an expected layout and a reported layout with identical values.
        let expected = ExpectedLayout { x: 100, y: 200, width: 300, height: 400 };
        let reported = reported_at(Some((100.0, 200.0)), (300, 400));

        // When: we compare them.
        let check = check_layout(&expected, &reported);

        // Then: the result is Match — no drift detected.
        assert_eq!(check, LayoutCheck::Match);
    }

    #[test]
    fn test_check_layout_subpixel_drift_within_tolerance() {
        // Given: a 0.5px drift on both axes — below the 1.0px threshold.
        let expected = ExpectedLayout { x: 100, y: 200, width: 300, height: 400 };
        let reported = reported_at(Some((100.5, 199.5)), (300, 400));

        // When: we compare.
        let check = check_layout(&expected, &reported);

        // Then: it's still a Match; sub-pixel noise must not trigger an eject.
        assert_eq!(check, LayoutCheck::Match);
    }

    #[test]
    fn test_check_layout_position_drift() {
        // Given: a 5px shift on x — clearly larger than rounding noise.
        let expected = ExpectedLayout { x: 100, y: 200, width: 300, height: 400 };
        let reported = reported_at(Some((105.0, 200.0)), (300, 400));

        // When: we compare.
        let check = check_layout(&expected, &reported);

        // Then: the result is Drift — caller should eject.
        assert_eq!(check, LayoutCheck::Drift);
    }

    #[test]
    fn test_check_layout_size_difference_is_not_drift() {
        // Given: position matches exactly but width has changed by 50.
        let expected = ExpectedLayout { x: 100, y: 200, width: 300, height: 400 };
        let reported = reported_at(Some((100.0, 200.0)), (350, 400));

        // When: we compare.
        let check = check_layout(&expected, &reported);

        // Then: Match. Size differences are deliberately ignored — apps
        // with min-size constraints (VS Code etc.) routinely refuse our
        // SetWindowWidth, and we don't want to eject them every reorder.
        // See the doc on `check_layout` for the full reasoning.
        assert_eq!(check, LayoutCheck::Match);
    }

    #[test]
    fn test_check_layout_position_at_threshold_drifts() {
        // Given: a position diff of exactly 1.0px (the threshold value).
        let expected = ExpectedLayout { x: 100, y: 200, width: 300, height: 400 };
        let reported = reported_at(Some((101.0, 200.0)), (300, 400));

        // When: we compare.
        let check = check_layout(&expected, &reported);

        // Then: at-threshold counts as Drift (`>= LAYOUT_TOLERANCE_PX`).
        assert_eq!(check, LayoutCheck::Drift);
    }

    #[test]
    fn test_check_layout_insufficient_when_pos_missing() {
        // Given: niri reports no workspace-view position for the window.
        let expected = ExpectedLayout { x: 100, y: 200, width: 300, height: 400 };
        let reported = reported_at(None, (300, 400));

        // When: we compare.
        let check = check_layout(&expected, &reported);

        // Then: result is Insufficient — caller should *not* eject on missing
        // info; we can't tell whether the user moved it.
        assert_eq!(check, LayoutCheck::Insufficient);
    }

    #[test]
    fn test_compute_layouts_returns_one_entry_per_panel_window() {
        // Given: two windows tracked on the right panel, both on the active
        // workspace (1).
        let w1 = mock_window(1, false, true, 1, None);
        let w2 = mock_window(2, false, true, 1, None);
        let state = AppState {
            right: panel_state_with(vec![win_state(1), win_state(2)]),
            ..Default::default()
        };

        // When: we compute layouts for a 1920x1080 screen.
        let layouts = compute_layouts(&mock_config(), &state, &[w1, w2], 1, (1920, 1080));

        // Then: we get one ExpectedLayout per tracked window, matching what
        // reorder would apply. N=2 → per_height = (980 - 10) / 2 = 485;
        // bottom (id=1) at y=545, top (id=2) at y=50.
        assert_eq!(layouts.len(), 2);
        assert_eq!(
            layouts.iter().find(|(id, _)| *id == 1).unwrap().1,
            ExpectedLayout { x: 1600, y: 545, width: 300, height: 485 }
        );
        assert_eq!(
            layouts.iter().find(|(id, _)| *id == 2).unwrap().1,
            ExpectedLayout { x: 1600, y: 50, width: 300, height: 485 }
        );
    }

    #[test]
    fn test_compute_layouts_skips_other_workspace_windows() {
        // Given: two tracked windows but only one on the active workspace.
        let w_here = mock_window(1, false, true, 1, None);
        let w_else = mock_window(2, false, true, 99, None);
        let state = AppState {
            right: panel_state_with(vec![win_state(1), win_state(2)]),
            ..Default::default()
        };

        // When: we compute layouts for workspace 1.
        let layouts = compute_layouts(&mock_config(), &state, &[w_here, w_else], 1, (1920, 1080));

        // Then: only the on-workspace window gets a layout, and it fills
        // the panel since n=1 from this workspace's perspective.
        assert_eq!(layouts.len(), 1);
        assert_eq!(layouts[0].0, 1);
        assert_eq!(layouts[0].1.height, 980);
    }

    #[test]
    fn test_compute_layouts_skips_disabled_panel() {
        // Given: a tracked window in the right panel, but the right panel is
        // disabled in config.
        let w1 = mock_window(1, false, true, 1, None);
        let mut config = mock_config();
        config.right.enabled = false;
        let state = AppState {
            right: panel_state_with(vec![win_state(1)]),
            ..Default::default()
        };

        // When: we compute layouts.
        let layouts = compute_layouts(&config, &state, &[w1], 1, (1920, 1080));

        // Then: the disabled panel contributes no layouts, so the result is
        // empty even though the window is tracked.
        assert!(layouts.is_empty());
    }

    #[test]
    fn test_compute_layouts_subtracts_top_and_bottom_bars() {
        // Given: a 30px top waybar (no bottom bar), one tracked window on the
        // right with the standard mock_config margins (top=50, bottom=50).
        let w1 = mock_window(1, false, true, 1, None);
        let mut config = mock_config();
        config.bars.top = 30;
        config.bars.bottom = 0;
        let state = AppState {
            right: panel_state_with(vec![win_state(1)]),
            ..Default::default()
        };

        // When: we compute layouts on a 1920x1080 output.
        let layouts = compute_layouts(&config, &state, &[w1], 1, (1920, 1080));

        // Then: the working area is 1080 - 30 - 0 = 1050. Available vertical
        // is 1050 - 50 - 50 = 950, so the single window gets height 950.
        // Bottom-up stack_y in working-area coords: 1050 - 50 - 950 = 50.
        // ExpectedLayout is in OUTPUT coords (matches what niri reports via
        // `tile_pos_in_workspace_view`), so we add bars.top: 50 + 30 = 80.
        // `apply_layouts` will subtract bars.top before sending to niri so
        // the action and the report round-trip cleanly.
        assert_eq!(layouts.len(), 1);
        let (_, layout) = layouts[0];
        assert_eq!(layout.height, 950);
        assert_eq!(layout.y, 80);
    }

    #[test]
    fn test_compute_layouts_with_zero_bars_matches_pre_bars_behavior() {
        // Given: bars defaulted to (0, 0) — what every prior test assumed.
        let w1 = mock_window(1, false, true, 1, None);
        let state = AppState {
            right: panel_state_with(vec![win_state(1)]),
            ..Default::default()
        };

        // When: we compute layouts.
        let layouts = compute_layouts(&mock_config(), &state, &[w1], 1, (1920, 1080));

        // Then: the math is identical to before bars existed — height 980
        // and y 50, matching `test_equal_height_one_window_fills_available`.
        // The new field is fully backwards-compatible when left at 0.
        assert_eq!(layouts.len(), 1);
        assert_eq!(layouts[0].1.height, 980);
        assert_eq!(layouts[0].1.y, 50);
    }

    #[test]
    fn test_reorder_sets_cooldown_on_window_whose_layout_will_change() {
        // Given: a tracked panel window whose niri-reported tile_pos is None
        // (so check_layout returns Insufficient — not a Match). The conservative
        // path treats this as "may animate" and sets cooldown.
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, None);
        let mock = MockNiri::new(vec![w1]);
        let state = AppState {
            right: panel_state_with(vec![win_state(1)]),
            ..Default::default()
        };
        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };
        let before_call = now_ms();

        // When: we run a full reorder.
        reorder(&mut ctx).expect("reorder failed");

        // Then: the window's cooldown_until is set to a time roughly
        // `cooldown_ms` (default 500) in the future. Slack covers clock
        // granularity and test execution time.
        let cooldown = ctx.state.right.windows[0]
            .cooldown_until
            .expect("cooldown must be set after a reorder");
        let expected_cooldown_lower = before_call + ctx.config.animation.cooldown_ms - 50;
        let expected_cooldown_upper = now_ms() + ctx.config.animation.cooldown_ms;
        assert!(
            cooldown >= expected_cooldown_lower && cooldown <= expected_cooldown_upper,
            "cooldown {cooldown} should land in [{expected_cooldown_lower}, {expected_cooldown_upper}]"
        );
    }

    #[test]
    fn test_reorder_with_bars_round_trips_position_consistently() {
        // Given: a 30px top bar, a window already at the resting position
        // niri would assign after our fix. Critically, the niri-reported
        // position (output coords) is `working_area_loc.y` higher than the
        // value we send via `MoveFloatingWindow`.
        let temp_dir = tempdir().unwrap();
        let mut w1 = mock_window(1, false, true, 1, None);
        // Output-coord position niri would report after our fix:
        //   working_h = 1080 - 30 = 1050
        //   stack_y_workarea = 1050 - 50 - 950 = 50
        //   output y = 50 + 30 = 80
        w1.layout.tile_pos_in_workspace_view = Some((1600.0, 80.0));
        w1.layout.window_size = (300, 950);
        let mock = MockNiri::new(vec![w1]);
        let mut config = mock_config();
        config.bars.top = 30;
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

        // When: we run a full reorder.
        reorder(&mut ctx).expect("reorder failed");

        // Then: niri's report (output y=80) matches our ExpectedLayout (y=80
        // since compute now produces output coords), so check_layout returns
        // Match and no cooldown is set. This is the case that *was* getting
        // a false Drift verdict and false cooldown set, blocking subsequent
        // drag-to-eject.
        assert_eq!(
            ctx.state.right.windows[0].cooldown_until,
            None,
            "no-op reorder with bars must not set cooldown"
        );
        // Apply must subtract bars before sending so niri ends up storing
        // the same output y after adding working_area_loc back.
        let actions = &ctx.socket.sent_actions;
        assert!(
            actions.iter().any(|a| matches!(a,
                Action::MoveFloatingWindow {
                    id: Some(1),
                    y: PositionChange::SetFixed(y),
                    ..
                } if (*y - 50.0).abs() < 0.5
            )),
            "apply_layouts must send working-area y (50), not output y (80)"
        );
    }

    #[test]
    fn test_reorder_skips_cooldown_when_layout_is_already_correct() {
        // Given: a window already at the position and size compute_layouts
        // would assign — niri.layout matches expected within ε. This is the
        // common case for focus-change-driven reorders, which fire on every
        // click but rarely actually change a panel window's layout.
        let temp_dir = tempdir().unwrap();
        let mut w1 = mock_window(1, false, true, 1, None);
        // mock_config: 1-window right panel layout is x=1600, y=50, w=300, h=980.
        w1.layout.tile_pos_in_workspace_view = Some((1600.0, 50.0));
        w1.layout.window_size = (300, 980);
        let mock = MockNiri::new(vec![w1]);
        let state = AppState {
            right: panel_state_with(vec![win_state(1)]),
            ..Default::default()
        };
        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        // When: we run a full reorder.
        reorder(&mut ctx).expect("reorder failed");

        // Then: cooldown stays None — niri won't emit any WLC events for a
        // no-op layout, so we have no animation frames to suppress.
        // Critically this means a user drag that follows a focus-change
        // reorder isn't blocked by a stale cooldown.
        assert_eq!(
            ctx.state.right.windows[0].cooldown_until,
            None,
            "cooldown must not be set when layout is already at expected"
        );
    }
}
