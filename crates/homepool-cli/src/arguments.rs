use std::{ffi::OsString, path::PathBuf};
pub const USAGE: &str = "Usage: homepool [--config FILE] [--socket FILE] COMMAND\nCommands: status [--json] [--garage-config FILE], hub, inventory [--json], place --json, release HANDLE, logs HANDLE [--tail N], add FILE, remove MEMBER, drain MEMBER, stage-storage MEMBER NODE ZONE --garage-config FILE, apply-storage VERSION --garage-config FILE, create-consumer ID --garage-config FILE --credentials-file FILE\nplace reads a JSON placement specification from stdin. See docs/hub.md.";

pub struct Options {
    pub config: Option<PathBuf>,
    pub socket: Option<PathBuf>,
    pub garage_config: Option<PathBuf>,
    pub credentials_file: Option<PathBuf>,
    pub storage_operands: Vec<OsString>,
    pub json: bool,
    pub tail: Option<usize>,
    pub command: String,
    pub operand: Option<OsString>,
}

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Options, String> {
    let mut args = args.into_iter();
    let mut options = Options {
        config: None,
        socket: None,
        garage_config: None,
        credentials_file: None,
        storage_operands: Vec::new(),
        json: false,
        tail: None,
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
        if [
            "--config",
            "--socket",
            "--garage-config",
            "--credentials-file",
        ]
        .iter()
        .any(|option| arg == *option)
        {
            let selected = if arg == "--config" {
                &mut options.config
            } else if arg == "--socket" {
                &mut options.socket
            } else if arg == "--garage-config" {
                &mut options.garage_config
            } else {
                &mut options.credentials_file
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
        } else if arg == "--tail" && options.tail.is_none() {
            let tail = args
                .next()
                .and_then(|v| v.to_str().and_then(|s| s.parse::<usize>().ok()))
                .ok_or("--tail requires an unsigned integer")?;
            if tail > crate::settings::LOG_MAX_TAIL {
                return Err("logs tail exceeds maximum".into());
            }
            options.tail = Some(tail);
        } else if arg == "--json" && !options.json {
            options.json = true;
        } else if options.command.is_empty()
            && [
                "status",
                "hub",
                "inventory",
                "place",
                "release",
                "logs",
                "add",
                "remove",
                "drain",
                "stage-storage",
                "apply-storage",
                "create-consumer",
            ]
            .iter()
            .any(|c| arg == *c)
        {
            options.command = arg.to_string_lossy().into_owned();
        } else if ["release", "logs", "add", "remove", "drain"].contains(&options.command.as_str())
            && options.operand.is_none()
            && !arg.to_string_lossy().starts_with('-')
        {
            options.operand = Some(arg);
        } else if is_storage_command(&options.command) && !arg.to_string_lossy().starts_with('-') {
            options.storage_operands.push(arg);
        } else {
            return Err("unexpected argument".into());
        }
    }
    if options.command.is_empty() {
        return Err(USAGE.into());
    }
    if ["release", "logs", "add", "remove", "drain"].contains(&options.command.as_str())
        && options.operand.is_none()
    {
        return Err("command requires an operand".into());
    }
    if options.json && !["status", "inventory", "place"].contains(&options.command.as_str()) {
        return Err("--json is only supported by status, inventory and place".into());
    }
    if options.tail.is_some() && options.command != "logs" {
        return Err("--tail is only supported by logs".into());
    }
    if options.command == "place" && !options.json {
        return Err("place requires --json".into());
    }
    if options.socket.is_some() && options.command == "status" {
        return Err("status reads declarations; use inventory for the hub".into());
    }
    if options.socket.is_some() && is_storage_command(&options.command) {
        return Err("--socket is not supported by storage commands".into());
    }
    if options.config.is_some()
        && !["status", "hub"].contains(&options.command.as_str())
        && !is_storage_command(&options.command)
    {
        return Err("--config is only supported by status, hub and storage commands".into());
    }
    if options.garage_config.is_some()
        && options.command != "status"
        && !is_storage_command(&options.command)
    {
        return Err("--garage-config is only supported by status and storage commands".into());
    }
    if options.credentials_file.is_some() && options.command != "create-consumer" {
        return Err("--credentials-file is only supported by create-consumer".into());
    }
    if is_storage_command(&options.command) {
        if options.garage_config.is_none() {
            return Err("storage commands require --garage-config FILE".into());
        }
        let expected = if options.command == "stage-storage" {
            3
        } else {
            1
        };
        if options.storage_operands.len() != expected {
            return Err("storage command requires its documented operands".into());
        }
        if options.command == "create-consumer" && options.credentials_file.is_none() {
            return Err("create-consumer requires --credentials-file FILE".into());
        }
    }
    Ok(options)
}

pub fn is_storage_command(command: &str) -> bool {
    ["stage-storage", "apply-storage", "create-consumer"].contains(&command)
}

#[cfg(test)]
mod tests {
    use super::parse;
    use std::ffi::OsString;

    #[test]
    fn logs_require_a_handle_and_validate_tail() {
        let args = |input: &[&str]| parse(input.iter().map(OsString::from));
        assert!(args(&["logs"]).is_err());
        assert_eq!(args(&["logs", "1:example"]).unwrap().tail, None);
        assert_eq!(
            args(&["logs", "1:example", "--tail", "0"]).unwrap().tail,
            Some(0)
        );
        assert_eq!(
            args(&["logs", "1:example", "--tail", "1000"]).unwrap().tail,
            Some(1000)
        );
        for tail in ["-1", "1001", "all", "18446744073709551616"] {
            assert!(args(&["logs", "1:example", "--tail", tail]).is_err());
        }
        assert!(args(&["logs", "1:example", "--tail"]).is_err());
        assert!(args(&["logs", "1:example", "--tail", "1", "--tail", "2"]).is_err());
        assert!(args(&["inventory", "--tail", "1"]).is_err());
    }
}
