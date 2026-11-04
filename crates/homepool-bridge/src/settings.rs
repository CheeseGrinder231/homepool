use std::time::Duration;
pub const SESSIONS: usize = 4;
pub const HANDSHAKE: Duration = Duration::from_secs(10);
pub const PROGRESS: Duration = Duration::from_secs(60);
pub const LIFETIME: Duration = Duration::from_secs(86400);
pub const PING: Duration = Duration::from_secs(15);
pub const PONG: Duration = Duration::from_secs(10);
pub const TICK: Duration = Duration::from_millis(50);
pub const INVITATION_SECONDS: u64 = 600;
pub const RETENTION: u64 = 30 * 86400;
pub const MAX_FILE: u64 = 50 * 1024 * 1024;
pub const QUOTA: u64 = 1024 * 1024 * 1024;
pub const TOTAL_QUOTA: u64 = 2 * 1024 * 1024 * 1024;
pub const SPOOL: u64 = 200 * 1024 * 1024;
pub const CREDIT: u64 = 8;
pub const MAX_DEVICES: usize = 64;
pub const MAX_INVITES: usize = 32;
pub const MAX_JOURNAL: usize = 10000;
pub const STATE_BYTES: u64 = 16 * 1024 * 1024;
pub const TOKEN_INTERVAL: Duration = Duration::from_millis(75);
pub const SETTLEMENT_RETRY: Duration = Duration::from_secs(1);
// Read-only service tabs (#59). One deadline covers connect, request and
// response; bodies stop at the service's max_bytes.
pub const SERVICE_DEADLINE: Duration = Duration::from_secs(15);
pub const SERVICE_DEFAULT_BYTES: u64 = 256 * 1024;
pub const SERVICE_MAX_BYTES: u64 = 1024 * 1024;
// The most a normalised document may be, as the page accepts
// (web/remote/view.mjs LIMITS.bytes). An independent bound: normalising can
// grow a document past its raw size, not only by `fetched_at`.
pub const SERVICE_SENT_BYTES: usize = 1024 * 1024 + 1024;
pub const SERVICE_HEADER_BYTES: usize = 16 * 1024;
pub const SERVICE_CHUNK_LINE: usize = 1024;
pub const SERVICE_TITLE_CHARS: usize = 40;
// View document v1 bounds; web/remote/view.mjs applies the same.
pub const VIEW_SECTIONS: usize = 20;
pub const VIEW_ITEMS: usize = 200;
pub const VIEW_NOTES: usize = 20;
pub const VIEW_META: usize = 8;
pub const VIEW_TEXT: usize = 500;
pub const VIEW_REFRESH_MIN: f64 = 30.0;
pub const VIEW_REFRESH_MAX: f64 = 3600.0;
pub const VIEW_REFRESH_DEFAULT: u64 = 60;

// Four successful keepalive intervals distinguish recovery from a short flap.
pub const RELAY_HEALTHY: Duration = Duration::from_secs(60);
