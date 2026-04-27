# niri-sidepanels

A lightweight, external sidepanel manager for the [niri](https://github.com/YaLTeR/niri) window manager.

`niri-sidepanels` lets you toggle any window into one of two independent floating sidepanel stacks — one anchored to the left edge of the screen, the other to the right. Stacks divide the available vertical space equally between their windows, so you can keep utility apps (terminals, music players, chats) accessible alongside the regular niri scrolling tape.

This project is a fork of [Vigintillionn/niri-sidebar](https://github.com/Vigintillionn/niri-sidebar), extended to support two independent panels instead of a single sidebar.

## Features

- **Dual independent panels:** A left and a right panel, each with its own width, margins, peek behavior, and window rules. Either can be enabled/disabled independently.
- **Equal-height stacking:** Windows in a panel divide the panel's vertical space equally, with a configurable gap between them.
- **Send across panels:** Move a window directly between panels, back to the niri tape, or detach it as a free-floating window.
- **Flip & hide:** Reverse the stacking order or hide a panel entirely (it peeks back in by a configurable amount so you know it's there).
- **Sticky panels:** Optionally have a panel's windows follow you across workspaces.
- **Window rules:** Auto-add specific applications (matched by `app_id` / `title` regex) to a chosen panel; override per-window width, peek, and focus_peek.
- **State persistence:** Tracking lists survive restarts.

## Installation

Build from source:

```bash
git clone https://github.com/zgavin/niri-sidepanels
cd niri-sidepanels
cargo build --release
cp target/release/niri-sidepanels ~/.local/bin/
```

## niri configuration

Add bindings to your niri `config.kdl`. The examples below assume the binary is at `~/.local/bin/niri-sidepanels`.

```kdl
binds {
    // Toggle the focused window into/out of a panel
    Mod+S         { spawn-sh "~/.local/bin/niri-sidepanels toggle-window right"; }
    Mod+A         { spawn-sh "~/.local/bin/niri-sidepanels toggle-window left"; }

    // Send the focused window to a specific destination (does not toggle)
    Mod+Shift+Right { spawn-sh "~/.local/bin/niri-sidepanels send right"; }
    Mod+Shift+Left  { spawn-sh "~/.local/bin/niri-sidepanels send left"; }
    // `center` un-floats and returns the window to the niri tape
    Mod+Shift+C     { spawn-sh "~/.local/bin/niri-sidepanels send center"; }
    // `floating` detaches from panel tracking but leaves the window floating
    Mod+Shift+F     { spawn-sh "~/.local/bin/niri-sidepanels send floating"; }

    // Hide / show a panel (peek still visible)
    Mod+Shift+S { spawn-sh "~/.local/bin/niri-sidepanels toggle-visibility right"; }
    Mod+Shift+A { spawn-sh "~/.local/bin/niri-sidepanels toggle-visibility left"; }

    // Reverse the stacking order of a panel
    Mod+Ctrl+S { spawn-sh "~/.local/bin/niri-sidepanels flip right"; }
    Mod+Ctrl+A { spawn-sh "~/.local/bin/niri-sidepanels flip left"; }

    // Force re-stacking on both panels (useful after manual nudges)
    Mod+Alt+R { spawn-sh "~/.local/bin/niri-sidepanels reorder"; }
}
```

To keep panels reactive to window events (close, focus change, workspace switch), spawn the listener daemon at startup:

```kdl
spawn-at-startup "~/.local/bin/niri-sidepanels" "listen"
```

Some applications enforce a minimum window size larger than your panel width, which can cause overlap. Add a niri window rule to bound them:

```kdl
window-rule {
    match is-floating=true
    min-width 100
    min-height 100
}
```

## Configuration

Run `niri-sidepanels init` to generate `~/.config/niri-sidepanels/config.toml` with defaults.

### Default config

```toml
# niri-sidepanels configuration
#
# Two independent panels are supported: `[left]` and `[right]`. Each one has
# its own width, margins, peek behavior, and window rules. Disable either by
# setting `enabled = false` in its section.

[left]
enabled = false
width = 400
# Deprecated: ignored since windows now split available vertical space
# equally. Kept in the schema for forward compatibility.
height = 335
# Gap between stacked windows, in pixels.
gap = 10
# How much of the panel peeks in from the edge when hidden, in pixels.
peek = 10
# How much of the panel is visible when a panel window is focused. Defaults
# to `peek`. Set equal to `width + margins.left` to fully unhide on focus.
focus_peek = 50
# Whether the panel's windows follow you when you switch workspaces.
sticky = false

[left.margins]
top = 50
right = 10
left = 10
bottom = 10

[right]
enabled = true
width = 400
height = 335
gap = 10
peek = 10
focus_peek = 50
sticky = false

[right.margins]
top = 50
right = 10
left = 10
bottom = 10
```

### Accounting for bars

If you have a layer-shell bar (waybar, etc.) at the top or bottom of the screen, niri's working area is smaller than the raw output. niri-sidepanels can't detect these zones automatically, so set them in `[bars]` to match — otherwise the daemon's height math thinks it has more vertical space than niri actually gives it, and panel windows extend past the visible bottom of the workspace.

```toml
[bars]
top = 30      # height of your top bar in pixels
bottom = 0    # height of your bottom bar; 0 if none
```

Both default to 0, so users without bars (or who haven't run into the issue) can leave the section out entirely.

### Window rules

Window rules let you customize behavior for specific windows by `app_id` or `title`. Rules are evaluated in order; the first match applies. Omitted fields fall back to the panel's defaults.

```toml
[[window_rule]]
app_id = "firefox"            # regex; if omitted, matches any app_id
title = "^Picture-in-Picture$" # regex; if omitted, matches any title
width = 700
peek = 10
focus_peek = 710
auto_add = true                # default false; auto-adds matching windows
side = "right"                 # which panel to auto-add to; defaults to right
```

Note: the `height` field on a window rule is ignored — panel windows divide the panel's vertical space equally regardless. Width still applies.

## Commands

| Command | Description |
| --- | --- |
| `toggle-window <left\|right>` | Toggle the focused window into or out of the named panel. Toggling out un-floats and returns it to the tape. |
| `send <left\|right\|center\|floating>` | Send the focused window to a specific destination (non-toggling). `center` un-floats back to the tape; `floating` detaches from panel tracking but leaves the window floating where it is. |
| `toggle-visibility <left\|right>` | Hide or show the named panel. Hidden panels still peek in. |
| `flip <left\|right>` | Reverse the stacking order of the named panel. |
| `focus <left\|right> [next\|prev]` | Cycle focus through windows in the named panel. |
| `reorder` | Force re-stacking of both panels. Useful after manual nudges. |
| `close` | Close the focused window; if it was on a panel, drop tracking. |
| `move-from <left\|right> <workspace>` | Move all windows tracked by the named panel from workspace N to the current workspace. |
| `init` | Write a default `config.toml` if one doesn't already exist. |
| `listen` | Run the daemon that watches niri events and keeps panels in sync. |

## Workflow tips

- **Adding/removing:** `Mod+S` snaps the focused window into the right panel; press again to send it back to the tape.
- **Cross-panel move:** `niri-sidepanels send left` (or right) on a panel-tracked window moves it across without un-floating.
- **Detach without re-tiling:** `niri-sidepanels send floating` pops a window out of its panel slot but keeps it floating at its current size — handy for one-off tasks where you don't want it back in the scrolling tape yet.
- **Hiding:** `Mod+Shift+S` tucks the right panel away. The configured `peek` keeps a thin sliver visible so you know it's there.

## License

MIT — see [LICENSE](LICENSE).
