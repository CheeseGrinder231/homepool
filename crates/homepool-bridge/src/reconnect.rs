use crate::settings;
use std::time::Duration;

pub struct Backoff(u64);
impl Backoff {
    pub fn new() -> Self {
        Self(1)
    }
    pub fn disconnected(&mut self, connected_for: Duration) {
        // Registration is a write, not proof the relay accepted it. Require a
        // minute of service (four keepalive intervals) before resetting retries.
        if connected_for >= settings::RELAY_HEALTHY {
            self.0 = 1;
        }
    }
    pub fn next(&mut self, jitter: u8) -> Duration {
        let delay = Duration::from_millis(self.0 * 1000 + u64::from(jitter));
        self.0 = (self.0 * 2).min(60);
        delay
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn short_connections_keep_backing_off_with_jitter_and_cap() {
        let mut b = Backoff::new();
        for seconds in [1, 2, 4, 8, 16, 32, 60, 60] {
            b.disconnected(settings::RELAY_HEALTHY - Duration::from_millis(1));
            assert_eq!(b.next(255), Duration::from_millis(seconds * 1000 + 255));
        }
    }
    #[test]
    fn sustained_connection_resets_retry_delay() {
        let mut b = Backoff::new();
        b.next(0);
        b.next(0);
        b.disconnected(settings::RELAY_HEALTHY);
        assert_eq!(b.next(0), Duration::from_secs(1));
        assert_eq!(b.next(0), Duration::from_secs(2));
    }
}
