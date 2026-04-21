use anyhow::Result;
use clap::{Parser, Subcommand};
use fslock::LockFile;
use niri_sidepanels::commands::Target;
use niri_sidepanels::config::{Side, load_config};
use niri_sidepanels::state::{get_default_cache_dir, load_state};
use niri_sidepanels::{AppState, Ctx, config, niri::connect};
use niri_sidepanels::{Direction, commands};

#[derive(Parser)]
#[command(name = "niri-sidepanels")]
#[command(about = "A dual floating sidepanel manager for Niri")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Toggle the focused window in/out of the given panel.
    ToggleWindow {
        #[arg(value_enum)]
        side: Side,
    },
    /// Send the focused window to a specific destination (left, right, or
    /// center). `center` means "remove from whichever panel it's on and
    /// return to the normal tiling tape". Unlike `toggle-window`, `send` is
    /// not toggling: a window already on `target` stays there.
    Send {
        #[arg(value_enum)]
        target: Target,
    },
    /// Hide or show the given panel.
    ToggleVisibility {
        #[arg(value_enum)]
        side: Side,
    },
    /// Reverse the order of windows in the given panel's stack.
    Flip {
        #[arg(value_enum)]
        side: Side,
    },
    /// Force re-stacking of windows on both panels.
    Reorder,
    /// Close the focused window. Removes it from whichever panel tracks it.
    Close,
    /// Focus-cycle through the windows in the given panel.
    Focus {
        #[arg(value_enum)]
        side: Side,
        #[arg(value_enum, default_value_t = Direction::Next)]
        direction: Direction,
    },
    /// Move a given panel's tracked windows from workspace N to the current one.
    MoveFrom {
        #[arg(value_enum)]
        side: Side,
        workspace: u64,
    },
    /// Generate a default config file if none exists.
    Init,
    /// Run a daemon to listen for window events.
    Listen,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Commands::Init = cli.command {
        return config::init_config();
    }

    let cache_dir = get_default_cache_dir()?;
    let mut lock_path = cache_dir.clone();
    lock_path.push("instance.lock");
    let mut lock_file = LockFile::open(&lock_path)?;

    if !matches!(cli.command, Commands::Listen) && !lock_file.try_lock()? {
        lock_file.lock()?;
    }
    let config = load_config();
    let state = if matches!(cli.command, Commands::Listen) {
        AppState::default()
    } else {
        load_state(&cache_dir)?
    };
    let socket = connect()?;

    let mut ctx = Ctx {
        state,
        config,
        socket,
        cache_dir,
    };

    match cli.command {
        Commands::ToggleWindow { side } => commands::toggle_window(&mut ctx, side)?,
        Commands::Send { target } => commands::send(&mut ctx, target)?,
        Commands::ToggleVisibility { side } => commands::toggle_visibility(&mut ctx, side)?,
        Commands::Flip { side } => commands::toggle_flip(&mut ctx, side)?,
        Commands::Reorder => commands::reorder(&mut ctx)?,
        Commands::Close => commands::close(&mut ctx)?,
        Commands::Focus { side, direction } => commands::focus(&mut ctx, side, direction)?,
        Commands::MoveFrom { side, workspace } => commands::move_from(&mut ctx, side, workspace)?,
        Commands::Init => unreachable!(),
        Commands::Listen => commands::listen(ctx)?,
    }

    Ok(())
}
