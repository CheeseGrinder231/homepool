use serde::Deserialize;

use crate::{error::Error, settings::DEFAULT_CPU_PERIOD_MICROS};

/// Only resource configuration is projected; never the container environment.
#[derive(Deserialize)]
pub(crate) struct Limits {
    pub(crate) nano_cpus: u64,
    pub(crate) cpu_quota: i64,
    pub(crate) cpu_period: u64,
    pub(crate) memory_bytes: u64,
}

impl Limits {
    pub(crate) fn cpu_millicores(&self) -> Result<Option<u64>, Error> {
        if self.cpu_quota < -1 {
            return Err(Error::Protocol("inspect limits"));
        }
        let nano = (self.nano_cpus > 0).then(|| self.nano_cpus.div_ceil(1_000_000));
        let quota = if self.cpu_quota > 0 {
            let period = if self.cpu_period == 0 {
                DEFAULT_CPU_PERIOD_MICROS
            } else {
                self.cpu_period
            };
            let value = (self.cpu_quota as u128 * 1_000).div_ceil(period as u128);
            Some(u64::try_from(value).map_err(|_| Error::Protocol("inspect limits"))?)
        } else {
            None
        };
        Ok(match (nano, quota) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        })
    }
}
