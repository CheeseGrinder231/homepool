mod arguments;
mod hub;
mod inventory;
mod private;
mod protocol;
mod settings;
mod status;
mod storage;

use protocol::Request;
use std::{
    env,
    io::{self, Read, Write},
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

fn run(interrupted: &Arc<AtomicUsize>) -> Result<(), String> {
    let options = arguments::parse(env::args_os().skip(1))?;
    let output = match options.command.as_str() {
        "help" => format!("{}\n", arguments::USAGE),
        "version" => format!("homepool {}\n", env!("CARGO_PKG_VERSION")),
        "status" | "hub" => {
            let path = settings::resolve_config(
                options.config,
                env::var_os("HOMEPOOL_CONFIG"),
                env::var_os("XDG_CONFIG_HOME"),
            )?;
            if options.command == "hub" {
                let socket = settings::resolve_socket(
                    options.socket,
                    env::var_os("HOMEPOOL_SOCKET"),
                    env::var_os("XDG_RUNTIME_DIR"),
                )?;
                hub::serve(&path, &socket)?;
                String::new()
            } else {
                let pool = settings::load(&path)?;
                let storage = options
                    .garage_config
                    .map(|config| storage::probe(config, interrupted))
                    .transpose()?;
                status::render(&pool, options.json, storage)?
            }
        }
        command if arguments::is_storage_command(command) => {
            storage::execute(&options, interrupted)?;
            String::new()
        }
        command => {
            let socket = settings::resolve_socket(
                options.socket,
                env::var_os("HOMEPOOL_SOCKET"),
                env::var_os("XDG_RUNTIME_DIR"),
            )?;
            let operand = options.operand.as_deref().unwrap_or_default();
            let member = || {
                operand
                    .to_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or_else(|| "member must be an unsigned integer".to_string())
            };
            let request = match command {
                "inventory" => Request::Inventory {},
                "place" => {
                    let mut input = Vec::new();
                    io::stdin()
                        .lock()
                        .take((settings::API_SIZE_LIMIT + 1) as u64)
                        .read_to_end(&mut input)
                        .map_err(|_| "could not read placement stdin")?;
                    if input.len() > settings::API_SIZE_LIMIT {
                        return Err("placement exceeds size limit".into());
                    }
                    Request::Place {
                        spec: serde_json::from_slice(&input)
                            .map_err(|_| "invalid placement JSON")?,
                    }
                }
                "release" => Request::Release {
                    handle: operand.to_str().ok_or("invalid handle")?.into(),
                },
                "add" => Request::Add {
                    document: private::read(std::path::Path::new(operand))?,
                },
                "drain" => Request::Drain { member: member()? },
                "remove" => Request::Remove { member: member()? },
                _ => return Err(arguments::USAGE.into()),
            };
            let result = hub::call(&socket, request)?;
            format!(
                "{}\n",
                serde_json::to_string(&result).map_err(|_| "could not encode output")?
            )
        }
    };
    if !output.is_empty() {
        io::stdout()
            .lock()
            .write_all(output.as_bytes())
            .map_err(|e| format!("stdout: {e}"))?;
    }
    Ok(())
}

fn main() -> ExitCode {
    let interrupted = Arc::new(AtomicUsize::new(0));
    let result = match run(&interrupted) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("homepool: {error}");
            ExitCode::FAILURE
        }
    };
    match interrupted.load(Ordering::SeqCst) as i32 {
        signal_hook::consts::SIGINT => ExitCode::from(130),
        signal_hook::consts::SIGTERM => ExitCode::from(143),
        _ => result,
    }
}
