//! Live storage probing retained from the merged Garage integration.
use signal_hook::{
    consts::{SIGINT, SIGTERM},
    flag,
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

pub fn probe(
    config: PathBuf,
    interrupted: &Arc<AtomicUsize>,
) -> Result<homepool_adapter_garage::StorageUsage, String> {
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
    let result = homepool_adapter_garage::Garage::new(&config)
        .with_cancellation(cancelled)
        .storage_usage();
    // Both success and error returns have completed child cleanup.
    terminate_immediately.store(true, Ordering::SeqCst);
    if interrupted.load(Ordering::SeqCst) != 0 {
        return Err(homepool_adapter_garage::Error::CANCELLED.to_string());
    }
    result.map_err(|e| e.to_string())
}
