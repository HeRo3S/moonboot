use clap::{Parser, Subcommand};
use moonboot::{
    backend::{self, Error, Event, Operation},
    config::Config,
    controller,
    demo::Scenario,
};
use std::{path::PathBuf, sync::atomic::Ordering};

#[derive(Parser)]
#[command(
    version,
    about = "Start Moonlight safely with a Tuya plug; power-off requires completed host shutdown",
    after_help = "Exit codes: 0 success; 2 configuration/dependency or invalid arguments; 3 cloud failure; 4 readiness timeout; 5 operation/UI busy; 6 Moonlight failure; 7 power-off declined; 130 cancelled."
)]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Print safe workflow diagnostics, never credentials or API responses.
    #[arg(long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Power on if needed, wait for Sunshine, and supervise Moonlight.
    Start,
    /// Validate dependencies and plug status without changing power.
    Check,
    /// Cut AC only after interactive confirmation of completed shutdown.
    PlugOff,
    /// Start an idle tray; no configuration or cloud access until controls open.
    Tray {
        #[arg(long, value_enum)]
        demo: Option<Scenario>,
    },
    /// Open or reopen the controls. Demo never reads production configuration.
    Gui {
        #[arg(long, value_enum)]
        demo: Option<Scenario>,
    },
}

fn run(cli: Cli) -> Result<(), Error> {
    // Reject pipes before configuration, credentials, locking, or cloud access.
    if matches!(cli.command, Commands::PlugOff) && unsafe { libc::isatty(libc::STDIN_FILENO) } != 1
    {
        return Err(Error::Config("plug-off requires interactive terminal input; use a terminal or moonboot gui. Piped approval is never accepted".into()));
    }
    let cancel = controller::signal_flag()?;
    let operation = match cli.command {
        Commands::Gui { demo } => return controller::gui(cli.config, demo, cancel),
        Commands::Tray { demo } => return controller::tray(cli.config, demo, cancel),
        Commands::Start => Operation::Start,
        Commands::Check => Operation::Check,
        Commands::PlugOff => Operation::PlugOff,
    };
    let config = Config::load(cli.config.as_deref())?;
    let result = backend::run(
        &config,
        operation,
        cancel.clone(),
        |event| match event {
            Event::Phase(phase) => println!("{}", moonboot::ui::phase_label(phase)),
            Event::Status(on) => println!(
                "Plug reports {}; this is not proof the PC is running",
                if on { "on" } else { "off" }
            ),
            Event::Message(message) => println!("{message}"),
        },
        || controller::terminal_approval(&config.moonlight.host, &config.tuya.device_id, &cancel),
    );
    if cli.verbose {
        eprintln!("Workflow finished; no power command is automatically reversed");
    }
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        result
    }
}

fn main() {
    let cli = Cli::parse();
    let frontend = matches!(cli.command, Commands::Gui { .. } | Commands::Tray { .. });
    let demo = match &cli.command {
        Commands::Gui { demo } | Commands::Tray { demo } => *demo,
        _ => None,
    };
    if let Err(error) = run(cli) {
        eprintln!("{error}");
        if frontend {
            controller::report_failure(&error, demo);
        }
        std::process::exit(error.code());
    }
}
