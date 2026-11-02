use crate::{
    settings,
    state::{self, Config, Entry, Result, State},
};
use homepool_protocol::wire::Transfer;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    sync::mpsc::{self, Receiver},
    time::Instant,
};
pub enum JobResult {
    Put,
    Head(Option<u64>),
    Get(File, u64, String, crate::limits::Reservation),
    List(Value),
}
pub struct Job {
    pub worker: Option<std::thread::JoinHandle<()>>,
    pub receiver: Receiver<Result<JobResult>>,
    pub started: Instant,
    pub cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl Job {
    pub fn poll(&mut self) -> Option<Result<JobResult>> {
        if !self.worker.as_ref()?.is_finished() {
            return None;
        }
        let joined = self.worker.take().unwrap().join();
        Some(if joined.is_err() {
            Err("store worker".into())
        } else {
            self.receiver
                .try_recv()
                .unwrap_or_else(|_| Err("store worker".into()))
        })
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}
pub struct Upload {
    pub hasher: Sha256,
    pub reservation: crate::limits::Reservation,
    pub journal: String,
    pub file: File,
    pub transfer: Transfer,
    pub credits: u64,
    pub progress: Instant,
}
pub struct Download {
    pub _reservation: crate::limits::Reservation,
    pub file: File,
    pub length: u64,
    pub sent: u64,
    pub count: u32,
    pub credits: u64,
    pub progress: Instant,
}
pub fn spool(c: &Config, key: &str) -> PathBuf {
    c.state_dir.join(format!("{key}.spool"))
}
pub fn create(c: &Config, key: &str) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(spool(c, key))
        .map_err(|_| "spool".into())
}
fn safe_name(name: &str) -> String {
    fn clean(s: &str, max: usize) -> String {
        let mut out = String::new();
        let mut replaced = false;
        for c in s.chars() {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                out.push(c);
                replaced = false;
            } else if !replaced {
                out.push('-');
                replaced = true;
            }
        }
        out.trim_matches(['.', '-'])
            .chars()
            .take(max)
            .collect::<String>()
            .trim_matches(['.', '-'])
            .to_owned()
    }
    let base = name.rsplit(['\\', '/']).next().unwrap_or("");
    let (stem, extension) = match base.rfind('.') {
        Some(i) if i > 0 => (&base[..i], &base[i + 1..]),
        _ => (base, ""),
    };
    let stem = clean(stem, 60);
    let stem = if stem.is_empty() { "image" } else { &stem };
    let extension = clean(extension, 12).to_ascii_lowercase();
    if extension.is_empty() {
        stem.to_owned()
    } else {
        format!("{stem}.{extension}")
    }
}
fn key(name: &str) -> Result<String> {
    let date = time::OffsetDateTime::now_utc();
    let f = time::format_description::parse_borrowed::<2>(
        "[year]-[month]-[day]/[hour][minute][second]",
    )
    .map_err(|_| "clock")?;
    let stamp = date.format(&f).map_err(|_| "clock")?;
    let bytes = state::unhex(&state::random(16)?)?;
    let alphabet = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut tag = String::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            tag.push(alphabet[((acc >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        tag.push(alphabet[((acc << (5 - bits)) & 31) as usize] as char);
    }
    Ok(format!("{stamp}-{tag}-{}", safe_name(name)))
}
pub fn begin(c: &Config, s: &mut State, device: &str, m: &Value) -> Result<Upload> {
    let reservation = crate::limits::Reservation::new(
        s.journal
            .values()
            .filter(|e| e.status == "pending")
            .map(|e| e.length)
            .sum(),
        false,
    )?;
    let entry = Entry {
        write_attempted: false,
        settled_at: None,
        committed_at: None,
        device: device.into(),
        id: m["id"].as_str().ok_or("id")?.into(),
        key: key(m["name"].as_str().ok_or("name")?)?,
        name: m["name"].as_str().ok_or("name")?.into(),
        mime: m["mime"].as_str().ok_or("mime")?.into(),
        length: m["length"].as_u64().ok_or("length")?,
        status: "pending".into(),
        hash: String::new(),
        created: state::now(),
    };
    let journal = s.reserve(&c.state_dir, entry.clone())?;
    let prepared = (|| {
        let file = create(c, &journal.replace(':', "-"))?;
        if entry.length > 0 {
            rustix::fs::fallocate(&file, rustix::fs::FallocateFlags::empty(), 0, entry.length)
                .map_err(|_| "spool reservation")?;
        }
        Ok(file)
    })();
    let file = match prepared {
        Ok(file) => file,
        Err(error) => {
            finish(c, s, &journal, false)?;
            return Err(error);
        }
    };
    Ok(Upload {
        hasher: Sha256::new(),
        reservation,
        journal,
        file,
        transfer: Transfer::new(entry.id, entry.length)?,
        credits: settings::CREDIT,
        progress: Instant::now(),
    })
}
pub fn chunk(upload: &mut Upload, m: &Value) -> Result<bool> {
    let end = upload.transfer.accept(m)?;
    if !end {
        if upload.credits == 0 {
            return Err("credit".into());
        }
        upload.credits -= 1;
        let bytes = m["data"]
            .as_array()
            .ok_or("data")?
            .iter()
            .map(|x| x.as_u64().unwrap() as u8)
            .collect::<Vec<_>>();
        upload.file.write_all(&bytes).map_err(|_| "spool")?;
        upload.hasher.update(&bytes);
        upload.progress = Instant::now();
    }
    Ok(end)
}
pub fn put(c: &Config, s: &mut State, upload: &mut Upload) -> Result<Job> {
    let store = c.store()?;
    upload.file.sync_all().map_err(|_| "spool sync")?;
    use std::io::Seek;
    upload.file.rewind().map_err(|_| "spool")?;
    let mut file = upload.file.try_clone().map_err(|_| "spool")?;
    let hash = state::hex(&upload.hasher.clone().finalize());
    let e = s.journal.get_mut(&upload.journal).ok_or("journal")?;
    e.hash = hash.clone();
    e.write_attempted = true;
    let entry = e.clone();
    // No PUT can begin before the uncertain-write marker is durable.
    s.save(&c.state_dir)?;
    let (sender, receiver) = mpsc::sync_channel(1);
    let cancelled = upload.reservation.cancelled.clone();
    let cancellation = cancelled.clone();
    let worker = std::thread::spawn(move || {
        let result = store
            .put(&entry.key, &mut file, &entry.mime, &hash, &cancellation)
            .map(|_| JobResult::Put)
            .map_err(Into::into);
        let _ = sender.send(result);
    });
    Ok(Job {
        receiver,
        started: Instant::now(),
        cancelled,
        worker: Some(worker),
    })
}
pub fn head(c: &Config, key: &str) -> Result<Job> {
    let store = c.store()?;
    let key = key.to_owned();
    let (sender, receiver) = mpsc::sync_channel(1);
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = std::thread::spawn(move || {
        let _ = sender.send(store.head(&key).map(JobResult::Head).map_err(Into::into));
    });
    Ok(Job {
        receiver,
        started: Instant::now(),
        cancelled,
        worker: Some(worker),
    })
}
// Called only after the writer is joined, or for an upload that never reached PUT.
pub fn finish(c: &Config, s: &mut State, journal: &str, committed: bool) -> Result<()> {
    let e = s.journal.get_mut(journal).ok_or("journal")?;
    e.status = if committed { "committed" } else { "failed" }.into();
    e.settled_at = Some(state::now());
    if committed {
        e.committed_at = Some(state::now());
    }
    s.save(&c.state_dir)?;
    cleanup(c, journal);
    Ok(())
}
fn cleanup(c: &Config, journal: &str) {
    match std::fs::remove_file(spool(c, &journal.replace(':', "-"))) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            eprintln!("spool cleanup failed; stopping bridge");
            std::process::exit(1);
        }
    }
}
pub fn get(c: &Config, s: &State, m: &Value) -> Result<Job> {
    let reservation = crate::limits::Reservation::new(
        s.journal
            .values()
            .filter(|e| e.status == "pending")
            .map(|e| e.length)
            .sum(),
        true,
    )?;
    let cancelled = reservation.cancelled.clone();
    let store = c.store()?;
    let key = m["key"].as_str().ok_or("key")?.to_owned();
    let name = state::random(16)?;
    let path = spool(c, &name);
    let mut file = create(c, &name)?;
    // Unlink immediately: the private open file is bounded and disappears on process exit.
    std::fs::remove_file(path).map_err(|_| "spool unlink")?;
    let (sender, receiver) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let result = (|| {
            let mut body = store.get(&key)?;
            let length = body.length;
            if length > settings::MAX_FILE {
                return Err("oversize".into());
            }
            if length > 0 {
                rustix::fs::fallocate(&file, rustix::fs::FallocateFlags::empty(), 0, length)
                    .map_err(|_| "spool reservation")?;
            }
            let mut buf = [0; 61440];
            loop {
                if reservation
                    .cancelled
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    return Err("cancelled".into());
                }
                let n = body.read(&mut buf).map_err(|_| "download")?;
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n]).map_err(|_| "download")?;
            }
            use std::io::Seek;
            file.rewind().map_err(|_| "spool")?;
            Ok(JobResult::Get(file, length, body.mime, reservation))
        })();
        let _ = sender.send(result);
    });
    Ok(Job {
        worker: Some(worker),
        receiver,
        started: Instant::now(),
        cancelled,
    })
}
pub fn list(c: &Config, s: &State, m: &Value) -> Result<Job> {
    let reservation = crate::limits::Reservation::new(
        s.journal
            .values()
            .filter(|e| e.status == "pending")
            .map(|e| e.length)
            .sum(),
        false,
    )?;
    let cancelled = reservation.cancelled.clone();
    let store = c.store()?;
    let cursor = m["cursor"].as_str().ok_or("cursor")?.to_owned();
    let limit = m["limit"].as_u64().ok_or("limit")? as u32;
    let id = m["id"].clone();
    let (sender, receiver) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let _reservation = reservation;
        let result=store.list(&cursor,limit).map_err(Into::into).map(|page|JobResult::List(json!({"type":"list-page","id":id,"cursor":page.next_continuation_token,"items":page.contents.into_iter().filter_map(|item| {
        let m=json!({"type":"get","id":id,"key":item.key});if homepool_protocol::wire::encode(&m).is_err(){None}else{Some(json!({"key":item.key,"length":item.size,"type":""}))}
    }).collect::<Vec<_>>()})));
        let _ = sender.send(result);
    });
    Ok(Job {
        worker: Some(worker),
        receiver,
        started: Instant::now(),
        cancelled,
    })
}
pub fn next(d: &mut Download, id: &str) -> Result<Option<Value>> {
    if d.sent == d.length {
        return Ok(Some(
            json!({"type":"get-end","id":id,"length":d.length,"count":d.count}),
        ));
    }
    if d.credits == 0 {
        return Ok(None);
    }
    let mut bytes = vec![0; 61440usize.min((d.length - d.sent) as usize)];
    d.file.read_exact(&mut bytes).map_err(|_| "download")?;
    let m = json!({"type":"chunk","id":id,"index":d.count,"data":bytes});
    d.sent += bytes.len() as u64;
    d.count += 1;
    d.credits -= 1;
    d.progress = Instant::now();
    Ok(Some(m))
}
pub fn reconcile(c: &Config, s: &mut State) -> Result<()> {
    let store = c.store()?;
    for e in s.journal.values_mut().filter(|e| e.status == "pending") {
        e.settled_at = Some(state::now());
        e.status = if e.write_attempted && store.head(&e.key)? == Some(e.length) {
            e.committed_at = Some(state::now());
            "committed"
        } else {
            "failed"
        }
        .into();
    }
    s.prune();
    s.save(&c.state_dir)?;
    for entry in std::fs::read_dir(&c.state_dir).map_err(|_| "spools")? {
        let entry = entry.map_err(|_| "spools")?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.ends_with(".spool"))
        {
            std::fs::remove_file(entry.path()).map_err(|_| "spool cleanup")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worker_result_is_not_exposed_until_the_writer_has_exited() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let (ready, observed) = mpsc::sync_channel(1);
        let (release, gate) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            sender.send(Ok(JobResult::Put)).unwrap();
            ready.send(()).unwrap();
            gate.recv().unwrap();
        });
        observed.recv().unwrap();
        let mut job = Job {
            receiver,
            started: Instant::now(),
            cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            worker: Some(worker),
        };
        assert!(job.poll().is_none());
        job.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        release.send(()).unwrap();
        let deadline = Instant::now() + settings::HANDSHAKE;
        while !job.worker.as_ref().unwrap().is_finished() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(matches!(job.poll(), Some(Ok(JobResult::Put))));
        assert!(job.worker.is_none());
    }
}
