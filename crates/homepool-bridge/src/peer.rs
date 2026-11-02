use crate::{
    operations::{self, Download, Job, JobResult, Upload},
    settings,
    state::{self, Config, Identity, Result, State},
};
use homepool_protocol::{Mode, Session, wire};
use serde_json::{Value, json};
use std::time::Instant;
pub struct Peer {
    expiry: Option<u64>,
    pub flush: bool,
    noise: Session,
    pub invitation: Option<String>,
    pub device: Option<String>,
    phase: u8,
    pub created: Instant,
    pub upload: Option<Upload>,
    pub download: Option<Download>,
    pub job: Option<Job>,
    pub request: String,
    pub journal: Option<String>,
    pub received: u64,
    pub acked: u64,
}
impl Peer {
    pub fn new(c: &Config, s: &State, identity: &Identity, binding: u32) -> Result<Self> {
        let invitation = s
            .invitations
            .values()
            .find(|i| i.binding == binding && i.expiry > state::now());
        let device = s.devices.values().find(|d| d.binding == binding);
        let expiry = invitation.map(|i| i.expiry);
        let (mode, psk, device, invitation) = if let Some(i) = invitation {
            if let Some(device) = &i.device {
                (
                    Mode::Reconnect,
                    None,
                    Some(device.clone()),
                    Some(i.id.clone()),
                )
            } else {
                (
                    Mode::Enroll,
                    Some(state::unhex(&i.psk)?),
                    None,
                    Some(i.id.clone()),
                )
            }
        } else if let Some(d) = device {
            (Mode::Reconnect, None, Some(d.id.clone()), None)
        } else {
            return Err("binding".into());
        };
        let noise = Session::new(
            mode,
            false,
            &c.route,
            &state::unhex(&identity.private)?,
            None,
            psk.as_deref(),
        )?;
        Ok(Self {
            expiry,
            flush: false,
            noise,
            invitation,
            device,
            phase: 0,
            created: Instant::now(),
            upload: None,
            download: None,
            job: None,
            request: String::new(),
            journal: None,
            received: 0,
            acked: 0,
        })
    }
    pub fn authenticated(&self) -> bool {
        self.phase == 2
    }
    pub fn encode(&mut self, m: &Value) -> Result<Vec<u8>> {
        self.noise.message(false, false, &wire::encode(m)?)
    }
    pub fn receive(&mut self, c: &Config, s: &mut State, bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
        if self.phase < 2 {
            if bytes.len() > 256 || self.created.elapsed() > settings::HANDSHAKE {
                return Err("handshake bounds".into());
            }
            self.noise.message(true, true, bytes)?;
            if self.device.is_some() {
                let device = s
                    .devices
                    .get(self.device.as_ref().unwrap())
                    .ok_or("revoked")?;
                if self.noise.remote_static() != Some(state::unhex(&device.key)?.as_slice()) {
                    return Err("device key".into());
                }
                let reply = self.noise.message(false, true, &[])?;
                self.phase = 2;
                let mut out = vec![reply];
                if self.invitation.is_some() {
                    out.push(
                        self.encode(
                            &json!({"type":"enrolled","id":device.id,"token":device.token}),
                        )?,
                    );
                }
                return Ok(out);
            }
            if self.phase == 0 {
                self.phase = 1;
                return Ok(vec![self.noise.message(false, true, &[])?]);
            }
            let key = state::hex(self.noise.remote_static().ok_or("device key")?);
            let device = s.enroll(
                &c.state_dir,
                self.invitation.as_ref().ok_or("invitation")?,
                key,
            )?;
            self.device = Some(device.id.clone());
            self.phase = 2;
            return Ok(vec![self.encode(
                &json!({"type":"enrolled","id":device.id,"token":device.token}),
            )?]);
        }
        if !s
            .devices
            .contains_key(self.device.as_ref().ok_or("device")?)
        {
            return Err("revoked".into());
        }
        let plaintext = self.noise.message(true, false, bytes)?;
        let m = wire::decode(&plaintext)?;
        self.flush = !matches!(m["type"].as_str(), Some("chunk" | "ping" | "pong"));
        let id = m["id"].as_str().ok_or("id")?;
        let result = self.operation(c, s, &m);
        let messages = match result {
            Ok(messages) => messages,
            Err(_) => vec![json!({"type":"error","id":id,"code":"refused"})],
        };
        self.flush |= messages.iter().any(|m| m["type"] == "credit");
        messages.iter().map(|m| self.encode(m)).collect()
    }
    fn operation(&mut self, c: &Config, s: &mut State, m: &Value) -> Result<Vec<Value>> {
        let id = m["id"].as_str().ok_or("id")?;
        let device = self.device.as_ref().ok_or("device")?;
        match m["type"].as_str().ok_or("type")? {
            "ping" => Ok(vec![json!({"type":"pong","id":id})]),
            "put-status" => {
                let e = s.journal.get(&format!("{device}:{id}"));
                Ok(vec![match e {
                    Some(e) if e.status == "committed" => {
                        json!({"type":"committed","id":id,"key":e.key,"name":e.name,"mime":e.mime,"length":e.length,"hash":e.hash})
                    }
                    Some(e) => json!({"type":e.status,"id":id}),
                    None => json!({"type":"unknown","id":id}),
                }])
            }
            "put-begin" | "get" | "list" => {
                if self.upload.is_some() || self.download.is_some() || self.job.is_some() {
                    return Err("busy".into());
                }
                self.request = id.into();
                self.journal = None;
                match m["type"].as_str().unwrap() {
                    "put-begin" => {
                        let u = operations::begin(c, s, device, m)?;
                        self.journal = Some(u.journal.clone());
                        self.upload = Some(u);
                        Ok(vec![
                            json!({"type":"credit","id":id,"chunks":settings::CREDIT}),
                        ])
                    }
                    "get" => {
                        self.job = Some(operations::get(c, s, m)?);
                        Ok(vec![])
                    }
                    _ => {
                        self.job = Some(operations::list(c, s, m)?);
                        Ok(vec![])
                    }
                }
            }
            "chunk" | "put-end" => {
                if id != self.request {
                    return Err("transfer id".into());
                }
                let upload = self.upload.as_mut().ok_or("transfer")?;
                if operations::chunk(upload, m)? {
                    let upload = self.upload.take().unwrap();
                    self.job = Some(operations::put(c, s, upload)?);
                    Ok(vec![])
                } else if upload.credits == 0 {
                    upload.credits = settings::CREDIT;
                    Ok(vec![
                        json!({"type":"credit","id":id,"chunks":settings::CREDIT}),
                    ])
                } else {
                    Ok(vec![])
                }
            }
            "credit" => {
                if id != self.request {
                    return Err("transfer id".into());
                }
                let d = self.download.as_mut().ok_or("transfer")?;
                let n = m["chunks"].as_u64().ok_or("credit")?;
                if d.credits + n > settings::CREDIT {
                    return Err("credit window".into());
                }
                d.credits += n;
                d.progress = Instant::now();
                Ok(vec![])
            }
            _ => Err("operation".into()),
        }
    }
    pub fn tick(&mut self, c: &Config, s: &mut State) -> Result<Vec<Vec<u8>>> {
        if self.expiry.is_some_and(|expiry| expiry <= state::now())
            || self.created.elapsed() > settings::LIFETIME
            || !self.authenticated() && self.created.elapsed() > settings::HANDSHAKE
            || self
                .upload
                .as_ref()
                .is_some_and(|u| u.progress.elapsed() > settings::PROGRESS)
            || self
                .download
                .as_ref()
                .is_some_and(|d| d.progress.elapsed() > settings::PROGRESS)
            || self
                .job
                .as_ref()
                .is_some_and(|j| j.started.elapsed() > settings::PROGRESS)
        {
            return Err("deadline".into());
        }
        let mut messages = vec![];
        if let Some(job) = &self.job {
            match job.receiver.try_recv() {
                Ok(result) => {
                    self.job = None;
                    match result {
                        Ok(JobResult::Put) => {
                            let e = s
                                .journal
                                .get_mut(self.journal.as_ref().ok_or("journal")?)
                                .ok_or("journal")?;
                            e.status = "committed".into();
                            e.committed_at = Some(state::now());
                            let key = e.key.clone();
                            s.save(&c.state_dir)?;
                            messages.push(json!({"type":"put-done","id":self.request,"key":key}));
                            let path = operations::spool(
                                c,
                                &self.journal.as_ref().unwrap().replace(':', "-"),
                            );
                            std::fs::remove_file(path).map_err(|_| "spool cleanup")?;
                        }
                        Ok(JobResult::Get(file, length, mime, reservation)) => {
                            self.download = Some(Download {
                                _reservation: reservation,
                                file,
                                length,
                                sent: 0,
                                count: 0,
                                credits: 0,
                                progress: Instant::now(),
                            });
                            messages.push(json!({"type":"get-head","id":self.request,"length":length,"mime":mime}));
                        }
                        Ok(JobResult::List(m)) => messages.push(m),
                        Err(_) => {
                            messages.push(json!({"type":"error","id":self.request,"code":"store"}))
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(_) => return Err("store worker".into()),
            }
        }
        if let Some(download) = &mut self.download
            && let Some(m) = operations::next(download, &self.request)?
        {
            let end = m["type"] == "get-end";
            messages.push(m);
            if end {
                self.download = None;
            }
        }
        messages.iter().map(|m| self.encode(m)).collect()
    }
}
