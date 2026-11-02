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
    Get(File, u64, crate::limits::Reservation),
    List(Value),
}
pub struct Job {
    pub receiver: Receiver<Result<JobResult>>,
    pub started: Instant,
    pub cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
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
fn key() -> Result<String> {
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
    Ok(format!("{stamp}-{tag}-image"))
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
        device: device.into(),
        id: m["id"].as_str().ok_or("id")?.into(),
        key: key()?,
        name: m["name"].as_str().ok_or("name")?.into(),
        mime: m["mime"].as_str().ok_or("mime")?.into(),
        length: m["length"].as_u64().ok_or("length")?,
        status: "pending".into(),
        hash: String::new(),
        created: state::now(),
    };
    let journal = s.reserve(&c.state_dir, entry.clone())?;
    let file = create(c, &journal.replace(':', "-"))?;
    if entry.length > 0 {
        rustix::fs::fallocate(&file, rustix::fs::FallocateFlags::empty(), 0, entry.length)
            .map_err(|_| "spool reservation")?;
    }
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
pub fn put(c: &Config, s: &mut State, mut upload: Upload) -> Result<Job> {
    upload.file.sync_all().map_err(|_| "spool sync")?;
    use std::io::Seek;
    upload.file.rewind().map_err(|_| "spool")?;
    let hash = state::hex(&upload.hasher.finalize());
    let e = s.journal.get_mut(&upload.journal).ok_or("journal")?;
    e.hash = hash.clone();
    let entry = e.clone();
    s.save(&c.state_dir)?;
    let store = c.store()?;
    let (sender, receiver) = mpsc::sync_channel(1);
    let cancelled = upload.reservation.cancelled.clone();
    std::thread::spawn(move || {
        let _reservation = upload.reservation;
        let result = store
            .put(
                &entry.key,
                &mut upload.file,
                &entry.mime,
                &hash,
                &_reservation.cancelled,
            )
            .map(|_| JobResult::Put)
            .map_err(Into::into);
        let _ = sender.send(result);
    });
    Ok(Job {
        receiver,
        started: Instant::now(),
        cancelled,
    })
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
    std::thread::spawn(move || {
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
            Ok(JobResult::Get(file, length, reservation))
        })();
        let _ = sender.send(result);
    });
    Ok(Job {
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
    std::thread::spawn(move || {
        let _reservation = reservation;
        let result=store.list(&cursor,limit).map_err(Into::into).map(|page|JobResult::List(json!({"type":"list-page","id":id,"cursor":page.next_continuation_token,"items":page.contents.into_iter().filter_map(|item| {
        let m=json!({"type":"get","id":id,"key":item.key});if homepool_protocol::wire::encode(&m).is_err(){None}else{Some(json!({"key":item.key,"length":item.size,"type":""}))}
    }).collect::<Vec<_>>()})));
        let _ = sender.send(result);
    });
    Ok(Job {
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
        e.status = if store.head(&e.key)? == Some(e.length) {
            "committed"
        } else {
            "failed"
        }
        .into();
    }
    s.journal.retain(|_, e| {
        state::now().saturating_sub(e.created) < settings::RETENTION || e.status == "pending"
    });
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
