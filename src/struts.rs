//! Manages the `layout { struts { left N right N } }` block in the user's niri
//! `config.kdl` so that the niri tape's reserved width tracks the sidepanels'
//! occupancy. The pure KDL manipulation (`update_struts_in_kdl`) is below;
//! `sync_struts_to_niri_config` is the daemon-side wrapper that reads the
//! file, computes desired values from current panel state, writes if the
//! result differs, and asks niri to reload its config.

use crate::config::{Panel, Side};
use crate::niri::NiriClient;
use crate::state::AppState;
use crate::Ctx;
use anyhow::{Context, Result};
use kdl::{KdlDocument, KdlDocumentFormat, KdlEntry};
use niri_ipc::Action;
use std::fs;
use std::path::{Path, PathBuf};

/// Suffix used for the one-time backup of the user's niri config the first
/// time we touch it.
const BACKUP_SUFFIX: &str = ".niri-sidepanels.bak";

/// Apply strut updates to a niri KDL config string and return the new string.
///
/// Preserves everything else — top/bottom struts, comments, ordering, the
/// user's other `layout` settings, etc. Each `(Side, value)` is upserted into
/// `layout > struts > <left|right> N`, with our marker comment as the trailing
/// annotation so the values are obviously tool-managed.
///
/// If the document has no `layout` node we add one. Same for `struts`. If
/// either or both already exist we edit in place.
pub fn update_struts_in_kdl(kdl_text: &str, updates: &[(Side, i32)]) -> Result<String> {
    if updates.is_empty() {
        return Ok(kdl_text.to_string());
    }

    // niri uses KDL v1 syntax (via the `knuffel` crate), not the v2 default
    // of the `kdl` crate. Use `parse_v1` for the input and `ensure_v1` +
    // `to_string` for the output so the round-trip stays in v1 form niri
    // can parse.
    let mut doc = KdlDocument::parse_v1(kdl_text)
        .map_err(|e| anyhow::anyhow!("failed to parse niri config as KDL: {e}"))?;

    // Ensure `layout > struts` exists with reasonable formatting. We seed any
    // missing scaffolding from a parsed template — that way KDL's own parser
    // gives us the right whitespace, terminator, and `before_children` glue
    // (constructing nested `KdlNode::new(...)`s manually produces ugly output
    // like `layout{` with no space).
    if doc.get("layout").is_none() {
        let scaffold = KdlDocument::parse_v1("layout {\n    struts {\n    }\n}\n")
            .expect("layout scaffold must be valid KDL");
        for node in scaffold.nodes() {
            doc.nodes_mut().push(node.clone());
        }
    }
    let layout = doc.get_mut("layout").expect("just ensured");
    let layout_children = layout.ensure_children();
    if layout_children.get("struts").is_none() {
        let scaffold = KdlDocument::parse_v1("    struts {\n    }\n")
            .expect("struts scaffold must be valid KDL");
        for node in scaffold.nodes() {
            layout_children.nodes_mut().push(node.clone());
        }
    }
    let struts = layout_children.get_mut("struts").expect("just ensured");
    let struts_children = struts.ensure_children();

    // When the struts block was just scaffolded its inner KdlDocument carries
    // whitespace in `leading`/`trailing` representing the gap between `{` and
    // `}`. Replace it with an explicit shape so per-node leading is the only
    // indentation that takes effect inside, and the closing `}` lines up with
    // the parent (`struts {` is itself indented 4 spaces, so `}` matches).
    if struts_children.nodes().is_empty() {
        struts_children.set_format(KdlDocumentFormat {
            leading: "\n".to_string(),
            trailing: "    ".to_string(),
        });
    }

    for (side, value) in updates {
        let key = match side {
            Side::Left => "left",
            Side::Right => "right",
        };
        upsert_managed_strut(struts_children, key, *value);
    }

    // Convert the document representation back to v1 syntax before
    // serializing, so niri (which only parses v1) can read the result.
    doc.ensure_v1();
    Ok(doc.to_string())
}

/// Set or replace the value of a `left`/`right` strut node. New nodes are
/// parsed from a marker-commented template (so KDL handles formatting). For
/// nodes that already exist we update only the integer value, leaving the
/// user's existing whitespace and any comments they wrote in place.
fn upsert_managed_strut(doc: &mut KdlDocument, key: &str, value: i32) {
    if let Some(existing) = doc.get_mut(key) {
        existing.entries_mut().clear();
        existing.push(KdlEntry::new(value as i128));
        return;
    }

    // New node: parse a template so spacing and terminator are handled by
    // KDL's own parser/serializer round-trip. v1 syntax for consistency
    // with niri's expectations (see `update_struts_in_kdl`).
    let template = format!("{key} {value}\n");
    let parsed = KdlDocument::parse_v1(&template)
        .expect("our own managed-strut template must always be valid KDL");
    let mut new_node = parsed
        .nodes()
        .first()
        .expect("template must contain exactly one node")
        .clone();

    // Match the leading indentation of an existing sibling so we line up with
    // siblings like `top 30` / `bottom 30`. KDL's `leading` field captures
    // *everything* between the previous terminator and the node — including
    // any leading newlines — so we want only the trailing run of spaces/tabs.
    // Default to 8 spaces (struts contents are at depth 2: layout > struts > X).
    let indent = doc
        .nodes()
        .iter()
        .find_map(|n| n.format().map(|f| f.leading.clone()))
        .as_deref()
        .map(line_indent_only)
        .unwrap_or_else(|| "        ".to_string());
    if let Some(format) = new_node.format_mut() {
        format.leading = indent;
    }

    doc.nodes_mut().push(new_node);
}

/// Strip everything up to (and including) the last newline in `s`, returning
/// just the trailing run of spaces/tabs that represents the line's indent.
fn line_indent_only(s: &str) -> String {
    s.rsplit('\n').next().unwrap_or(s).to_string()
}

/// Locate the user's niri config file. Honors `XDG_CONFIG_HOME`, falls back to
/// `~/.config/niri/config.kdl`.
pub fn locate_niri_config() -> Result<PathBuf> {
    let mut path = dirs::config_dir().context("could not resolve XDG config dir")?;
    path.push("niri");
    path.push("config.kdl");
    Ok(path)
}

/// Make a single one-time backup of `path` at `path + BACKUP_SUFFIX` if no
/// backup exists yet. Idempotent — subsequent calls are no-ops, so we never
/// overwrite the user's untouched original.
pub fn backup_if_first_time(path: &Path) -> Result<()> {
    let mut backup = path.as_os_str().to_owned();
    backup.push(BACKUP_SUFFIX);
    let backup = PathBuf::from(backup);
    if backup.exists() {
        return Ok(());
    }
    if !path.exists() {
        // Nothing to back up; the caller is creating the config from scratch.
        return Ok(());
    }
    fs::copy(path, &backup).with_context(|| format!("failed to back up {path:?} to {backup:?}"))?;
    Ok(())
}

/// Write `content` to `path` atomically: write to a sibling temp file then
/// rename over the destination. Avoids leaving a half-written niri config if
/// the process is killed mid-write.
pub fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let parent = path
        .parent()
        .context("config path has no parent directory")?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("config path has no file name")?;
    let tmp = parent.join(format!(".{file_name}.niri-sidepanels.tmp"));
    fs::write(&tmp, content).with_context(|| format!("failed to write temp file {tmp:?}"))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename {tmp:?} → {path:?}"))?;
    Ok(())
}

/// Compute the desired strut values for each managed side, given panel
/// config and current state.
///
/// - Side with `panel.strut == None` → not managed, no entry in result.
/// - Side empty → `panel.strut`.
/// - Side non-empty → `panel.strut + panel.width`.
pub(crate) fn desired_struts(config: &crate::config::Config, state: &AppState) -> Vec<(Side, i32)> {
    let mut out = Vec::new();
    for side in Side::ALL {
        let panel: &Panel = config.panel(side);
        let Some(base) = panel.strut else { continue };
        let value = if state.panel(side).windows.is_empty() {
            base
        } else {
            base + panel.width
        };
        out.push((side, value));
    }
    out
}

/// Read the user's niri config, compute the desired strut block from
/// current panel state, write it back if different, and ask niri to
/// reload. No-op when no side has `strut` configured (the user hasn't
/// opted in to managed struts at all).
///
/// Best-effort throughout: locate / read / write / reload errors are
/// surfaced via `Result` but the daemon's caller can choose to log and
/// continue rather than abort the whole reorder.
pub fn sync_struts_to_niri_config<C: NiriClient>(ctx: &mut Ctx<C>) -> Result<()> {
    let updates = desired_struts(&ctx.config, &ctx.state);
    if updates.is_empty() {
        return Ok(()); // No side opted in.
    }

    let path = locate_niri_config()?;
    let current = if path.exists() {
        fs::read_to_string(&path)
            .with_context(|| format!("failed to read niri config at {path:?}"))?
    } else {
        String::new()
    };

    let new_content = update_struts_in_kdl(&current, &updates)?;
    if new_content == current {
        return Ok(()); // Already correct; no write, no reload.
    }

    backup_if_first_time(&path)?;
    // If the config didn't exist before (fresh install), make sure the
    // parent directory does.
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create niri config dir {parent:?}"))?;
    }
    atomic_write(&path, &new_content)?;

    // Ask niri to reload the file we just wrote. Errors here mean niri's
    // socket rejected the action — log and continue rather than failing
    // the user's command. (Most likely cause: niri isn't running, in which
    // case the next start will pick up the updated config anyway.)
    if let Err(e) = ctx.socket.send_action(Action::LoadConfigFile {}) {
        eprintln!("niri-sidepanels: failed to ask niri to reload config: {e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn assert_left_right(kdl: &str, expected_left: i32, expected_right: i32) {
        let doc: KdlDocument = kdl.parse().expect("output must reparse cleanly");
        let layout = doc.get("layout").expect("layout block must exist");
        let struts = layout
            .children()
            .expect("layout must have children")
            .get("struts")
            .expect("struts node must exist");
        let struts_doc = struts.children().expect("struts must have children");
        let l = struts_doc.get("left").unwrap().entries()[0].value().as_integer().unwrap();
        let r = struts_doc.get("right").unwrap().entries()[0].value().as_integer().unwrap();
        assert_eq!(l, expected_left as i128);
        assert_eq!(r, expected_right as i128);
    }

    #[test]
    fn test_update_struts_in_empty_config_creates_layout_block() {
        // Given: an empty niri config with no layout block at all.
        let input = "";

        // When: we apply struts for both sides.
        let output =
            update_struts_in_kdl(input, &[(Side::Left, 100), (Side::Right, 200)]).unwrap();

        // Then: a layout > struts block is created with both values.
        assert_left_right(&output, 100, 200);
    }

    #[test]
    fn test_update_struts_no_updates_returns_input_unchanged() {
        // Given: a config that already has user content.
        let input = "input-binds {\n    Mod+Q { quit; }\n}\n";

        // When: we call with no updates.
        let output = update_struts_in_kdl(input, &[]).unwrap();

        // Then: the output is exactly the input — never touch the file when
        // the caller has nothing to apply.
        assert_eq!(output, input);
    }

    #[test]
    fn test_update_struts_preserves_top_and_bottom_struts() {
        // Given: an existing layout block with the user's own top/bottom struts.
        let input = "layout {\n    struts {\n        top 50\n        bottom 30\n    }\n}\n";

        // When: we set left/right.
        let output = update_struts_in_kdl(input, &[(Side::Left, 400), (Side::Right, 400)]).unwrap();

        // Then: top/bottom survive untouched alongside the new left/right.
        let doc: KdlDocument = output.parse().unwrap();
        let struts = doc
            .get("layout").unwrap()
            .children().unwrap()
            .get("struts").unwrap()
            .children().unwrap();
        assert_eq!(struts.get("top").unwrap().entries()[0].value().as_integer().unwrap(), 50);
        assert_eq!(struts.get("bottom").unwrap().entries()[0].value().as_integer().unwrap(), 30);
        assert_eq!(struts.get("left").unwrap().entries()[0].value().as_integer().unwrap(), 400);
        assert_eq!(struts.get("right").unwrap().entries()[0].value().as_integer().unwrap(), 400);
    }

    #[test]
    fn test_update_struts_preserves_unrelated_layout_settings() {
        // Given: a layout block with non-strut settings that we don't manage.
        let input = "layout {\n    gaps 16\n    center-focused-column \"never\"\n}\n";

        // When: we add a strut.
        let output = update_struts_in_kdl(input, &[(Side::Right, 400)]).unwrap();

        // Then: gaps and center-focused-column are still present.
        let doc: KdlDocument = output.parse().unwrap();
        let layout = doc.get("layout").unwrap().children().unwrap();
        assert!(layout.get("gaps").is_some(), "unrelated layout settings must survive");
        assert!(layout.get("center-focused-column").is_some());
        assert_eq!(
            layout.get("struts").unwrap().children().unwrap().get("right").unwrap().entries()[0]
                .value()
                .as_integer()
                .unwrap(),
            400
        );
    }

    #[test]
    fn test_update_struts_replaces_existing_managed_value() {
        // Given: a config where we previously wrote `right 300`.
        let input = "layout {\n    struts {\n        right 300\n    }\n}\n";

        // When: we update right to 500.
        let output = update_struts_in_kdl(input, &[(Side::Right, 500)]).unwrap();

        // Then: the old value is gone, the new one is written, and we don't
        // append a duplicate `right` node.
        let doc: KdlDocument = output.parse().unwrap();
        let struts_doc = doc
            .get("layout").unwrap()
            .children().unwrap()
            .get("struts").unwrap()
            .children().unwrap();
        let right_count = struts_doc.nodes().iter().filter(|n| n.name().value() == "right").count();
        assert_eq!(right_count, 1, "must not duplicate the `right` node");
        assert_eq!(
            struts_doc.get("right").unwrap().entries()[0].value().as_integer().unwrap(),
            500
        );
    }

    #[test]
    fn test_update_struts_only_one_side() {
        // Given: an empty config.
        let input = "";

        // When: only the right side is updated (left is omitted from updates).
        let output = update_struts_in_kdl(input, &[(Side::Right, 400)]).unwrap();

        // Then: only the right strut is written; left is absent. We never
        // touch a side the caller didn't ask us to touch.
        let doc: KdlDocument = output.parse().unwrap();
        let struts_doc = doc
            .get("layout").unwrap()
            .children().unwrap()
            .get("struts").unwrap()
            .children().unwrap();
        assert!(struts_doc.get("right").is_some());
        assert!(struts_doc.get("left").is_none(), "left must remain unmanaged");
    }

    #[test]
    fn test_update_struts_supports_negative_values() {
        // Given: an empty config and a request for a negative strut (extends
        // the workspace area past the screen edge — niri allows this).
        let input = "";

        // When: we apply the negative value.
        let output = update_struts_in_kdl(input, &[(Side::Left, -20)]).unwrap();

        // Then: the value is preserved as-is, including the sign.
        let doc: KdlDocument = output.parse().unwrap();
        let l = doc
            .get("layout").unwrap()
            .children().unwrap()
            .get("struts").unwrap()
            .children().unwrap()
            .get("left").unwrap()
            .entries()[0]
            .value()
            .as_integer()
            .unwrap();
        assert_eq!(l, -20);
    }

    #[test]
    fn test_update_struts_round_trips_complex_config() {
        // Given: a realistic niri config snippet with multiple top-level
        // sections, comments, and existing user struts.
        let input = r#"
// User's niri config

input {
    keyboard {
        xkb {
            layout "us"
        }
    }
}

layout {
    gaps 16
    struts {
        top 30
        bottom 30
    }
    center-focused-column "always"
}

binds {
    Mod+Q { quit; }
}
"#;

        // When: we set left/right struts.
        let output =
            update_struts_in_kdl(input, &[(Side::Left, 400), (Side::Right, 400)]).unwrap();

        // Then: the entire document still parses and all original sections
        // (input, layout's gaps/center, binds) remain intact, with new
        // struts appended to the existing struts block.
        let doc: KdlDocument = output.parse().expect("output must still parse");
        assert!(doc.get("input").is_some(), "unrelated `input` section preserved");
        assert!(doc.get("binds").is_some(), "unrelated `binds` section preserved");
        let layout_children = doc.get("layout").unwrap().children().unwrap();
        assert!(layout_children.get("gaps").is_some());
        assert!(layout_children.get("center-focused-column").is_some());
        assert_left_right(&output, 400, 400);
    }

    #[test]
    fn test_backup_first_time_creates_bak_file() {
        // Given: an existing niri config file with no backup yet.
        let dir = tempdir().unwrap();
        let cfg = dir.path().join("config.kdl");
        fs::write(&cfg, "original content").unwrap();

        // When: we run the first-time backup.
        backup_if_first_time(&cfg).unwrap();

        // Then: the .niri-sidepanels.bak sibling exists with the original content.
        let bak = dir.path().join("config.kdl.niri-sidepanels.bak");
        assert!(bak.exists());
        assert_eq!(fs::read_to_string(&bak).unwrap(), "original content");
    }

    #[test]
    fn test_backup_does_not_overwrite_existing_bak() {
        // Given: a config that has already been edited (current content
        // differs from the existing .bak).
        let dir = tempdir().unwrap();
        let cfg = dir.path().join("config.kdl");
        let bak = dir.path().join("config.kdl.niri-sidepanels.bak");
        fs::write(&bak, "pristine original").unwrap();
        fs::write(&cfg, "now-edited content").unwrap();

        // When: backup_if_first_time runs again.
        backup_if_first_time(&cfg).unwrap();

        // Then: the .bak still holds the pristine version — we must never
        // clobber the one snapshot of the user's untouched config.
        assert_eq!(fs::read_to_string(&bak).unwrap(), "pristine original");
    }

    #[test]
    fn test_atomic_write_replaces_file_in_place() {
        // Given: an existing file with old contents.
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.kdl");
        fs::write(&path, "old").unwrap();

        // When: we atomic-write new content.
        atomic_write(&path, "new content").unwrap();

        // Then: the destination has the new content and no temp file remains.
        assert_eq!(fs::read_to_string(&path).unwrap(), "new content");
        let leftover_tmp = dir.path().join(".config.kdl.niri-sidepanels.tmp");
        assert!(!leftover_tmp.exists(), "temp file must be renamed away");
    }

    use crate::config::{Config, Panel};
    use crate::state::{PanelState, WindowState};

    fn ws(id: u64) -> WindowState {
        WindowState {
            id,
            width: 100,
            height: 100,
            is_floating: false,
            position: None,
            cooldown_until: None,
            last_applied: None,
        }
    }

    #[test]
    fn test_desired_struts_no_managed_sides_returns_empty() {
        // Given: a config with `strut = None` on both sides (the user has
        // not opted in to managed struts at all).
        let config = Config::default();
        let state = AppState::default();

        // When: we compute desired struts.
        let updates = desired_struts(&config, &state);

        // Then: no entries — the helper short-circuits without touching
        // the niri config at all.
        assert!(updates.is_empty());
    }

    #[test]
    fn test_desired_struts_empty_panel_uses_base() {
        // Given: right panel managed with `strut = 20`, no windows tracked.
        let mut config = Config::default();
        config.right = Panel {
            enabled: true,
            width: 400,
            strut: Some(20),
            ..Panel::default()
        };
        let state = AppState::default();

        // When: we compute desired struts.
        let updates = desired_struts(&config, &state);

        // Then: just the base value — no panel.width added because there's
        // nothing in the panel.
        assert_eq!(updates, vec![(Side::Right, 20)]);
    }

    #[test]
    fn test_desired_struts_non_empty_panel_adds_width() {
        // Given: right panel managed with `strut = 20`, one tracked window.
        let mut config = Config::default();
        config.right = Panel {
            enabled: true,
            width: 400,
            strut: Some(20),
            ..Panel::default()
        };
        let mut state = AppState::default();
        state.right = PanelState {
            windows: vec![ws(1)],
            is_hidden: false,
            is_flipped: false,
        };

        // When: we compute desired struts.
        let updates = desired_struts(&config, &state);

        // Then: 20 + 400 = 420. Panel windows + base padding.
        assert_eq!(updates, vec![(Side::Right, 420)]);
    }

    #[test]
    fn test_desired_struts_only_includes_managed_sides() {
        // Given: left managed (strut Some), right unmanaged (strut None).
        let mut config = Config::default();
        config.left = Panel {
            enabled: true,
            width: 400,
            strut: Some(0),
            ..Panel::default()
        };
        config.right = Panel {
            enabled: true,
            width: 400,
            strut: None,
            ..Panel::default()
        };
        let mut state = AppState::default();
        state.left = PanelState {
            windows: vec![ws(1)],
            is_hidden: false,
            is_flipped: false,
        };
        // Right has windows too, but we're not managing its strut.
        state.right = PanelState {
            windows: vec![ws(2)],
            is_hidden: false,
            is_flipped: false,
        };

        // When: we compute desired struts.
        let updates = desired_struts(&config, &state);

        // Then: only the left entry. Right is left untouched in niri's
        // config — the user owns it.
        assert_eq!(updates, vec![(Side::Left, 400)]);
    }

    #[test]
    fn test_update_struts_lowers_value_when_panel_empties() {
        // Given: niri config where we previously wrote a non-empty-panel
        // strut value of 500 (= 100 base + 400 panel width).
        let input = "layout {\n    struts {\n        left 500\n    }\n}\n";

        // When: we update with the empty-panel value (just the base).
        let output = update_struts_in_kdl(input, &[(Side::Left, 100)]).unwrap();

        // Then: the strut comes down to the base. This is the
        // toggle-window-out → reorder → sync path. Catches a regression
        // where the value would only ever go *up* but not back down.
        let doc: KdlDocument = output.parse().unwrap();
        let l = doc
            .get("layout").unwrap()
            .children().unwrap()
            .get("struts").unwrap()
            .children().unwrap()
            .get("left").unwrap()
            .entries()[0]
            .value()
            .as_integer()
            .unwrap();
        assert_eq!(l, 100, "value must come back down when panel empties");
    }

    #[test]
    fn test_desired_struts_supports_zero_and_negative_base() {
        // Given: right panel with strut = 0 (just panel width, no padding)
        // and one tracked window. niri allows negative struts too — they
        // extend the working area past the screen edge.
        let mut config = Config::default();
        config.right = Panel {
            enabled: true,
            width: 400,
            strut: Some(0),
            ..Panel::default()
        };
        let mut state = AppState::default();
        state.right = PanelState {
            windows: vec![ws(1)],
            is_hidden: false,
            is_flipped: false,
        };

        // When/Then: zero base, non-empty → just panel.width.
        assert_eq!(desired_struts(&config, &state), vec![(Side::Right, 400)]);

        // And: negative base, empty → write the negative value through.
        config.right.strut = Some(-50);
        state.right.windows.clear();
        assert_eq!(desired_struts(&config, &state), vec![(Side::Right, -50)]);
    }
}
