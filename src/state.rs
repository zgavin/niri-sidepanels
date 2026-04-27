use crate::config::Side;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct AppState {
    #[serde(default)]
    pub left: PanelState,
    #[serde(default)]
    pub right: PanelState,
    #[serde(default)]
    pub ignored_windows: Vec<u64>,
}

impl AppState {
    pub fn panel(&self, side: Side) -> &PanelState {
        match side {
            Side::Left => &self.left,
            Side::Right => &self.right,
        }
    }

    pub fn panel_mut(&mut self, side: Side) -> &mut PanelState {
        match side {
            Side::Left => &mut self.left,
            Side::Right => &mut self.right,
        }
    }

    /// Return the side that contains the window with the given id, if any.
    pub fn side_of(&self, id: u64) -> Option<Side> {
        if self.left.windows.iter().any(|w| w.id == id) {
            Some(Side::Left)
        } else if self.right.windows.iter().any(|w| w.id == id) {
            Some(Side::Right)
        } else {
            None
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct PanelState {
    #[serde(default)]
    pub windows: Vec<WindowState>,
    #[serde(default)]
    pub is_hidden: bool,
    #[serde(default)]
    pub is_flipped: bool,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct WindowState {
    pub id: u64,
    pub width: i32,
    pub height: i32,
    pub is_floating: bool,
    pub position: Option<(f64, f64)>,
    /// Unix-millis timestamp until which drift checks should be skipped for
    /// this window — we recently sent it reorder actions and niri is still
    /// animating to the new layout. `None` means no cooldown active.
    /// `#[serde(default)]` so existing on-disk state files stay readable
    /// after the schema change.
    #[serde(default)]
    pub cooldown_until: Option<i64>,
}

pub fn get_default_cache_dir() -> Result<PathBuf> {
    let mut path = dirs::cache_dir().context("Could not find cache directory")?;
    path.push("niri-sidepanels");
    if !path.exists() {
        fs::create_dir_all(&path)?;
    }
    Ok(path)
}

pub fn load_state(base_dir: &Path) -> Result<AppState> {
    let mut path = base_dir.to_path_buf();
    path.push("state.json");
    if path.exists() {
        let content = fs::read_to_string(path)?;
        Ok(serde_json::from_str(&content).unwrap_or_default())
    } else {
        Ok(AppState::default())
    }
}

pub fn save_state(state: &AppState, base_dir: &Path) -> Result<()> {
    let mut path = base_dir.to_path_buf();
    path.push("state.json");
    let content = serde_json::to_string_pretty(state)?;
    fs::write(path, content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_save_and_load_roundtrip() {
        let temp_dir = tempdir().unwrap();

        let w1 = WindowState {
            id: 100,
            width: 500,
            height: 400,
            is_floating: false,
            position: None,
            cooldown_until: None,
        };
        let w2 = WindowState {
            id: 200,
            width: 1920,
            height: 1080,
            is_floating: true,
            position: Some((1.0, 2.0)),
            cooldown_until: None,
        };

        let original_state = AppState {
            left: PanelState {
                windows: vec![w1],
                is_hidden: false,
                is_flipped: false,
            },
            right: PanelState {
                windows: vec![w2],
                is_hidden: true,
                is_flipped: true,
            },
            ignored_windows: vec![100, 200],
        };

        save_state(&original_state, temp_dir.path()).expect("Failed to save state");
        let loaded_state = load_state(temp_dir.path()).expect("Failed to load state");

        assert_eq!(original_state, loaded_state);

        let mut expected_path = temp_dir.path().to_path_buf();
        expected_path.push("state.json");
        assert!(expected_path.exists());
    }

    #[test]
    fn test_load_defaults_if_no_file() {
        let temp_dir = tempdir().unwrap();
        let state = load_state(temp_dir.path()).expect("Should not fail on missing file");
        assert_eq!(state, AppState::default());
        assert!(state.left.windows.is_empty());
        assert!(state.right.windows.is_empty());
    }

    #[test]
    fn test_handles_corrupted_json() {
        let temp_dir = tempdir().unwrap();
        let mut path = temp_dir.path().to_path_buf();
        path.push("state.json");
        fs::write(&path, "{ bad_json: ").unwrap();

        let state = load_state(temp_dir.path()).expect("Should recover from bad JSON");
        assert_eq!(state, AppState::default());
    }

    #[test]
    fn test_side_of_finds_window() {
        let mut state = AppState::default();
        state.left.windows.push(WindowState {
            id: 1,
            width: 100,
            height: 100,
            is_floating: false,
            position: None,
            cooldown_until: None,
        });
        state.right.windows.push(WindowState {
            id: 2,
            width: 100,
            height: 100,
            is_floating: false,
            position: None,
            cooldown_until: None,
        });

        assert_eq!(state.side_of(1), Some(Side::Left));
        assert_eq!(state.side_of(2), Some(Side::Right));
        assert_eq!(state.side_of(99), None);
    }
}
