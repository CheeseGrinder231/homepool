use std::{ffi::OsString, path::PathBuf};
pub const USAGE: &str = "Usage: homepool [--config FILE] [--socket FILE] COMMAND\nCommands: status [--json], hub, inventory [--json], place --json, release HANDLE, add FILE, remove MEMBER, drain MEMBER\nplace reads a JSON placement specification from stdin. See docs/hub.md.";

pub struct Options {
    pub config: Option<PathBuf>,
    pub socket: Option<PathBuf>,
    pub json: bool,
    pub command: String,
    pub operand: Option<OsString>,
}

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Options, String> {
    let mut args = args.into_iter();
    let mut options = Options {
        config: None,
        socket: None,
        json: false,
        command: String::new(),
        operand: None,
    };
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            options.command = "help".into();
            return Ok(options);
        }
        if arg == "--version" {
            options.command = "version".into();
            return Ok(options);
        }
        if arg == "--config" || arg == "--socket" {
            let selected = if arg == "--config" {
                &mut options.config
            } else {
                &mut options.socket
            };
            if selected.is_some() {
                return Err(format!(
                    "{} may only be supplied once",
                    arg.to_string_lossy()
                ));
            }
            let value = args
                .next()
                .ok_or_else(|| format!("{} requires a file path", arg.to_string_lossy()))?;
            if value.is_empty() || value.to_string_lossy().starts_with('-') {
                return Err(format!("{} requires a file path", arg.to_string_lossy()));
            }
            *selected = Some(PathBuf::from(value));
        } else if arg == "--json" && !options.json {
            options.json = true;
        } else if options.command.is_empty()
            && [
                "status",
                "hub",
                "inventory",
                "place",
                "release",
                "add",
                "remove",
                "drain",
            ]
            .iter()
            .any(|c| arg == *c)
        {
            options.command = arg.to_string_lossy().into_owned();
        } else if ["release", "add", "remove", "drain"].contains(&options.command.as_str())
            && options.operand.is_none()
            && !arg.to_string_lossy().starts_with('-')
        {
            options.operand = Some(arg);
        } else {
            return Err(format!("unexpected argument: {}", arg.to_string_lossy()));
        }
    }
    if options.command.is_empty() {
        return Err(USAGE.into());
    }
    if ["release", "add", "remove", "drain"].contains(&options.command.as_str())
        && options.operand.is_none()
    {
        return Err("command requires an operand".into());
    }
    if options.json && !["status", "inventory", "place"].contains(&options.command.as_str()) {
        return Err("--json is only supported by status, inventory and place".into());
    }
    if options.command == "place" && !options.json {
        return Err("place requires --json".into());
    }
    if options.socket.is_some() && options.command == "status" {
        return Err("status reads declarations; use inventory for the hub".into());
    }
    if options.config.is_some() && !["status", "hub"].contains(&options.command.as_str()) {
        return Err("--config is only supported by status and hub".into());
    }
    Ok(options)
}
