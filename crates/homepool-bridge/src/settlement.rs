//! Owns PUT workers independently of phone sessions. Pending reservations stay
//! alive through cancellation, worker exit, and any uncertain-outcome HEAD.
use crate::{
    operations::{self, Job, JobResult, Upload},
    settings,
    state::{Config, Result, State},
};
use std::{collections::BTreeMap, time::Instant};
enum Phase {
    Put,
    Head,
    Retry,
}
struct Write {
    upload: Upload,
    job: Option<Job>,
    phase: Phase,
    retry: Instant,
}
#[derive(Default)]
pub struct Settlements {
    writes: BTreeMap<String, Write>,
}
impl Settlements {
    pub fn start(&mut self, c: &Config, s: &mut State, mut upload: Upload) -> Result<()> {
        match operations::put(c, s, &mut upload) {
            Ok(job) => {
                self.writes.insert(
                    upload.journal.clone(),
                    Write {
                        upload,
                        job: Some(job),
                        phase: Phase::Put,
                        retry: Instant::now(),
                    },
                );
                Ok(())
            }
            Err(error) => {
                Self::abort(c, s, upload)?;
                Err(error)
            }
        }
    }
    pub fn abort(c: &Config, s: &mut State, upload: Upload) -> Result<()> {
        if s.journal
            .get(&upload.journal)
            .is_none_or(|e| e.write_attempted)
        {
            return Err("uncertain upload requires reconciliation".into());
        }
        operations::finish(c, s, &upload.journal, false)?;
        // The file and reservation are released only after durable settlement and unlink.
        drop(upload);
        Ok(())
    }
    pub fn cancel(&mut self, journal: &str) {
        if let Some(write) = self.writes.get_mut(journal) {
            write
                .upload
                .reservation
                .cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    pub fn tick(&mut self, c: &Config, s: &mut State) -> Result<()> {
        let mut finished = vec![];
        for (journal, write) in &mut self.writes {
            if let Some(result) = write.job.as_mut().and_then(Job::poll) {
                write.job = None; // poll joined the writer before inspecting the fixed key.
                match (&write.phase, result) {
                    (Phase::Put, Ok(JobResult::Put)) => {
                        finished.push((journal.clone(), true));
                        continue;
                    }
                    (Phase::Head, Ok(JobResult::Head(length))) => {
                        finished.push((journal.clone(), length == Some(s.journal[journal].length)));
                        continue;
                    }
                    _ => {
                        write.phase = Phase::Retry;
                        write.retry = Instant::now() + settings::SETTLEMENT_RETRY;
                    }
                }
            }
            if matches!(write.phase, Phase::Retry) && Instant::now() >= write.retry {
                match operations::head(c, &s.journal[journal].key) {
                    Ok(job) => {
                        write.job = Some(job);
                        write.phase = Phase::Head;
                    }
                    Err(_) => write.retry = Instant::now() + settings::SETTLEMENT_RETRY,
                }
            }
        }
        for (journal, committed) in finished {
            operations::finish(c, s, &journal, committed)?;
            self.writes.remove(&journal); // releases its slot after the spool is removed
        }
        Ok(())
    }
}
