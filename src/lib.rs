pub mod commands;
pub mod config;
pub mod niri;
pub mod state;
pub mod struts;
pub mod window_rules;

use std::path::PathBuf;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

pub use crate::config::{Config, Side};
pub use crate::niri::NiriClient;
pub use crate::state::AppState;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

pub struct Ctx<C: NiriClient> {
    pub state: AppState,
    pub config: Config,
    pub socket: C,
    pub cache_dir: PathBuf,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Debug)]
pub enum Direction {
    Next,
    Prev,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowTarget {
    pub width: i32,
    pub height: i32,
}

/// What the daemon thinks a panel window should look like right now. Computed
/// from config + state + niri's current window list. Lives at lib.rs so it
/// can be both produced/consumed by `commands::reorder` and stored in
/// `state::WindowState` (`last_applied`) without a circular module
/// dependency.
///
/// Coordinates are in **output-relative space** (matching niri's
/// `tile_pos_in_workspace_view` reports for floating windows). `apply_layouts`
/// translates back to working-area coords before sending niri actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedLayout {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}
