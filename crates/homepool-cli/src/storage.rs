//! Direct owner operations on an existing Garage installation.
use crate::{arguments::Options, private, settings};
use homepool_adapter_garage::{Garage, StorageUsage};
use homepool_core::MemberId;
use signal_hook::{
    consts::{SIGINT, SIGTERM},
    flag,
};
use std::{
    env,
    ffi::OsStr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

pub fn probe(config: PathBuf, interrupted: &Arc<AtomicUsize>) -> Result<StorageUsage, String> {
    with_garage(config, interrupted, |garage| {
        garage.storage_usage().map_err(|e| e.to_string())
    })
}

pub fn execute(options: &Options, interrupted: &Arc<AtomicUsize>) -> Result<(), String> {
    let path = settings::resolve_config(
        options.config.clone(),
        env::var_os("HOMEPOOL_CONFIG"),
        env::var_os("XDG_CONFIG_HOME"),
    )?;
    let config = private::load(&path)?;
    let garage_config = options
        .garage_config
        .clone()
        .ok_or("missing Garage configuration")?;
    let number = |value: &OsStr| {
        value
            .to_str()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| "storage identity or version must be an unsigned integer".to_string())
    };
    let id = number(&options.storage_operands[0])?;
    match options.command.as_str() {
        "stage-storage" => {
            let member = config
                .pool
                .member(MemberId(id))
                .map_err(|_| "unknown storage member")?;
            let node = options.storage_operands[1]
                .to_str()
                .ok_or("invalid Garage node ID")?;
            let zone = options.storage_operands[2]
                .to_str()
                .ok_or("invalid Garage zone")?;
            with_garage(garage_config, interrupted, |garage| {
                garage
                    .stage_portion(member, node, zone)
                    .map_err(|e| e.to_string())
            })
        }
        "apply-storage" => with_garage(garage_config, interrupted, |garage| {
            garage.apply_layout(id).map_err(|e| e.to_string())
        }),
        "create-consumer" => {
            let path = options
                .credentials_file
                .as_deref()
                .ok_or("missing credentials destination")?;
            // Reserve a new private regular file before any Garage call. A guard
            // removes the empty/partial file on failure; an existing path is never opened.
            let mut destination = private::NewFile::create(path)?;
            with_garage(garage_config, interrupted, |garage| {
                let consumer = garage.create_consumer(id).map_err(|e| e.to_string())?;
                let saved = (|| {
                    let document = serde_json::to_vec(&serde_json::json!({
                        "version": 1,
                        "bucket": consumer.bucket,
                        "aws_access_key_id": consumer.credentials.access_key_id(),
                        "aws_secret_access_key": consumer.credentials.secret_access_key(),
                    }))
                    .map_err(|_| ())?;
                    destination.persist(&document).map_err(|_| ())
                })();
                saved.map_err(|_| "Garage issued credentials but the credentials file could not be written; the consumer may exist; inspect Garage privately before retrying".to_string())
            })
        }
        _ => Err("unknown storage command".into()),
    }
}

fn with_garage<T>(
    config: PathBuf,
    interrupted: &Arc<AtomicUsize>,
    operation: impl FnOnce(&Garage) -> Result<T, String>,
) -> Result<T, String> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let terminate_immediately = Arc::new(AtomicBool::new(true));
    for signal in [SIGINT, SIGTERM] {
        // Record the exit code before publishing cancellation.
        flag::register_conditional_default(signal, Arc::clone(&terminate_immediately))
            .and_then(|_| flag::register_usize(signal, Arc::clone(interrupted), signal as usize))
            .and_then(|_| flag::register(signal, Arc::clone(&cancelled)))
            .map_err(|_| "could not register Garage cancellation signals".to_string())?;
    }
    // Defer ordinary termination only while child cleanup is needed.
    terminate_immediately.store(false, Ordering::SeqCst);
    // A signal between the two registrations must also cancel.
    if interrupted.load(Ordering::SeqCst) != 0 {
        cancelled.store(true, Ordering::Release);
    }
    let garage = Garage::new(&config).with_cancellation(cancelled);
    let result = operation(&garage);
    // Both success and error returns have completed child cleanup.
    terminate_immediately.store(true, Ordering::SeqCst);
    if interrupted.load(Ordering::SeqCst) != 0 {
        return Err(homepool_adapter_garage::Error::CANCELLED.to_string());
    }
    result
}
