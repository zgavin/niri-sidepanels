use anyhow::{Context, Result};
use clap::ValueEnum;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

pub const DEFAULT_CONFIG_STR: &str = include_str!("../default_config.toml");

#[derive(
    Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, ValueEnum, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
}

impl Side {
    pub const ALL: [Side; 2] = [Side::Left, Side::Right];

    pub fn other(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub left: Panel,
    #[serde(default = "default_right_panel")]
    pub right: Panel,
    #[serde(default)]
    pub window_rule: Vec<WindowRule>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            left: Panel::default(),
            right: default_right_panel(),
            window_rule: vec![],
        }
    }
}

impl Config {
    pub fn panel(&self, side: Side) -> &Panel {
        match side {
            Side::Left => &self.left,
            Side::Right => &self.right,
        }
    }

    pub fn panel_mut(&mut self, side: Side) -> &mut Panel {
        match side {
            Side::Left => &mut self.left,
            Side::Right => &mut self.right,
        }
    }

    /// Refuse to act on a panel that's disabled in config. Use this at the
    /// start of every command that targets a side, so we surface a clear
    /// error rather than silently leaving an orphaned floating window when a
    /// keybind hits a disabled panel.
    pub fn require_enabled(&self, side: Side) -> Result<&Panel> {
        let panel = self.panel(side);
        if !panel.enabled {
            let name = match side {
                Side::Left => "left",
                Side::Right => "right",
            };
            anyhow::bail!(
                "panel '{name}' is disabled in config — enable it under [{name}] to use this command"
            );
        }
        Ok(panel)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Panel {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_width")]
    pub width: i32,
    #[serde(default = "default_height")]
    pub height: i32,
    #[serde(default = "default_gap")]
    pub gap: i32,
    #[serde(default)]
    pub margins: Margins,
    #[serde(default = "default_peek")]
    pub peek: i32,
    pub focus_peek: Option<i32>,
    #[serde(default)]
    pub sticky: bool,
}

impl Default for Panel {
    fn default() -> Self {
        Self {
            enabled: false,
            width: default_width(),
            height: default_height(),
            gap: default_gap(),
            margins: Margins::default(),
            peek: default_peek(),
            focus_peek: None,
            sticky: false,
        }
    }
}

impl Panel {
    pub fn get_focus_peek(&self) -> i32 {
        self.focus_peek.unwrap_or(self.peek)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Margins {
    #[serde(default)]
    pub top: i32,
    #[serde(default)]
    pub right: i32,
    #[serde(default)]
    pub left: i32,
    #[serde(default)]
    pub bottom: i32,
}

impl Default for Margins {
    fn default() -> Self {
        Self {
            top: 0,
            right: 0,
            left: 0,
            bottom: 0,
        }
    }
}

fn default_right_panel() -> Panel {
    Panel {
        enabled: true,
        ..Panel::default()
    }
}

fn default_width() -> i32 {
    400
}

fn default_height() -> i32 {
    335
}

fn default_gap() -> i32 {
    10
}

fn default_peek() -> i32 {
    10
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct WindowRule {
    #[serde(default, with = "serde_regex")]
    pub app_id: Option<Regex>,
    #[serde(default, with = "serde_regex")]
    pub title: Option<Regex>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub peek: Option<i32>,
    pub focus_peek: Option<i32>,
    #[serde(default)]
    pub auto_add: bool,
    /// Optional side affinity for auto_add rules. When None, auto_add falls
    /// back to the right panel if it's enabled, else left.
    pub side: Option<Side>,
}

pub fn get_config_dir() -> Result<PathBuf> {
    let mut path = dirs::config_dir().context("Could not find config directory")?;
    path.push("niri-sidepanels");
    Ok(path)
}

pub fn load_config() -> Config {
    let Ok(mut path) = get_config_dir() else {
        return Config::default();
    };
    path.push("config.toml");

    if path.exists() {
        if let Ok(content) = fs::read_to_string(&path) {
            match toml::from_str(&content) {
                Ok(cfg) => return cfg,
                Err(e) => eprintln!("Error parsing config.toml: {}. Using defaults.", e),
            }
        }
    }
    toml::from_str(DEFAULT_CONFIG_STR).expect("Default config file is invalid TOML")
}

pub fn init_config() -> Result<()> {
    let mut path = get_config_dir()?;

    if !path.exists() {
        fs::create_dir_all(&path)?;
        println!("Created directory: {:?}", path);
    }

    path.push("config.toml");

    if path.exists() {
        anyhow::bail!("Config file already exists at {:?}", path);
    }

    fs::write(&path, DEFAULT_CONFIG_STR)?;
    println!("Default config written to {:?}", path);
    Ok(())
}
