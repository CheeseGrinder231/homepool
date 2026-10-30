mod settings;
mod status;

use std::{
    env,
    ffi::OsString,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

const USAGE: &str = "Usage: homepool [--config FILE] status [--json]\n\nRead the private member file and show its declared members and budgets.\n--config FILE may also follow status. See docs/members.md for the schema.";

struct Options {
    config: Option<PathBuf>,
    json: bool,
}

enum Command {
    Status(Options),
    Help,
    Version,
}

fn arguments(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut args = args.into_iter();
    let mut config = None;
    let mut json = false;
    let mut status = false;
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            return Ok(Command::Help);
        } else if arg == "--version" {
            return Ok(Command::Version);
        } else if arg == "--config" {
            if config.is_some() {
                return Err("--config may only be supplied once".into());
            }
            let value = args.next().ok_or("--config requires a file path")?;
            if value.is_empty() || value.to_string_lossy().starts_with('-') {
                return Err("--config requires a file path".into());
            }
            config = Some(PathBuf::from(value));
        } else if arg == "--json" && !json {
            json = true;
        } else if arg == "status" && !status {
            status = true;
        } else {
            return Err(format!("unexpected argument: {}", arg.to_string_lossy()));
        }
    }
    if !status {
        return Err(USAGE.into());
    }
    Ok(Command::Status(Options { config, json }))
}

fn run() -> Result<(), String> {
    let output = match arguments(env::args_os().skip(1))? {
        Command::Help => format!("{USAGE}\n"),
        Command::Version => format!("homepool {}\n", env!("CARGO_PKG_VERSION")),
        Command::Status(options) => {
            let path = settings::resolve_config(
                options.config,
                env::var_os("HOMEPOOL_CONFIG"),
                env::var_os("XDG_CONFIG_HOME"),
            )?;
            let pool = settings::load(&path)?;
            status::render(&pool, options.json)?
        }
    };
    io::stdout()
        .lock()
        .write_all(output.as_bytes())
        .map_err(|e| format!("stdout: {e}"))
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("homepool: {error}");
            ExitCode::FAILURE
        }
    }
}
