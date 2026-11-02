use crate::settings;
type Result<T> = std::result::Result<T, String>;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
static SLOTS: AtomicUsize = AtomicUsize::new(0);
static DOWNLOAD_BYTES: AtomicU64 = AtomicU64::new(0);
pub struct Reservation {
    download: bool,
    pub cancelled: Arc<AtomicBool>,
}
impl Reservation {
    pub fn new(pending: u64, download: bool) -> Result<Self> {
        SLOTS
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < settings::SESSIONS).then_some(n + 1)
            })
            .map_err(|_| "active transfer limit")?;
        if download
            && DOWNLOAD_BYTES
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                    (n + pending + settings::MAX_FILE <= settings::SPOOL)
                        .then_some(n + settings::MAX_FILE)
                })
                .is_err()
        {
            SLOTS.fetch_sub(1, Ordering::SeqCst);
            return Err("spool limit".into());
        }
        Ok(Self {
            download,
            cancelled: Arc::new(AtomicBool::new(false)),
        })
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        SLOTS.fetch_sub(1, Ordering::SeqCst);
        if self.download {
            DOWNLOAD_BYTES.fetch_sub(settings::MAX_FILE, Ordering::SeqCst);
        }
    }
}
pub fn download_bytes() -> u64 {
    DOWNLOAD_BYTES.load(Ordering::SeqCst)
}
