//! Approved jobs use Docker identities as their only durable record.
use crate::{
    hub, private,
    protocol::{Placement, Request},
    settings::*,
};
use homepool_adapter_docker::Environment;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path, sync::Mutex, time::Instant};

#[derive(Default)]
pub struct Jobs {
    operations: BTreeMap<String, Operation>,
    submissions: Mutex<()>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u64,
    operations: Vec<Operation>,
}
// No Debug implementation: these values can contain secrets.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Operation {
    name: String,
    image: String,
    command: Vec<String>,
    cpu_millicores: u64,
    memory_bytes: u64,
    #[serde(default)]
    environment: BTreeMap<String, String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Submission {
    request_id: String,
    operation: String,
}
#[derive(Serialize)]
struct Job<'a> {
    request_id: &'a str,
    operation: Option<&'a str>,
    state: &'a str,
    exit_code: Option<i64>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Logs {
    stdout: String,
    stderr: String,
    cut: bool,
}
#[derive(Serialize)]
struct JobLogs<'a> {
    request_id: &'a str,
    operation: &'a str,
    state: &'a str,
    #[serde(flatten)]
    logs: Logs,
}

pub fn valid_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}
fn valid_operation_name(value: &str) -> bool {
    valid_identity(value) && value.len() <= OPERATION_NAME_BYTES
}
fn prefix(request_id: &str) -> String {
    format!("api-{:x}-", Sha256::digest(request_id.as_bytes()))
}
impl Jobs {
    pub fn load(path: Option<&Path>) -> Result<Self, String> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let load = || {
            let bytes = private::read_bounded(path, OPERATIONS_BYTES).ok()?;
            let document: Document = toml::from_str(std::str::from_utf8(&bytes).ok()?).ok()?;
            if document.version != 1 {
                return None;
            }
            let mut jobs = Self::default();
            for operation in document.operations {
                if !valid_operation_name(&operation.name)
                    || operation.image.is_empty()
                    || operation.image.starts_with('-')
                    || operation.image.contains('\0')
                    || operation.command.iter().any(|a| a.contains('\0'))
                    || operation.cpu_millicores == 0
                    || !operation.cpu_millicores.is_multiple_of(CPU_QUANTUM)
                    || operation.cpu_millicores > i64::MAX as u64 / 1_000_000
                    || operation.memory_bytes == 0
                    || operation.memory_bytes > i64::MAX as u64
                {
                    return None;
                }
                let mut environment = Environment::default();
                for (key, value) in &operation.environment {
                    environment.insert(key, value).ok()?;
                }
                if jobs
                    .operations
                    .insert(operation.name.clone(), operation)
                    .is_some()
                {
                    return None;
                }
            }
            Some(jobs)
        };
        load().ok_or_else(|| "invalid API operations file".into())
    }
    pub fn names(&self) -> Result<Vec<u8>, u16> {
        serde_json::to_vec(&self.operations.keys().collect::<Vec<_>>()).map_err(|_| 503)
    }
    pub fn get(
        &self,
        socket: &Path,
        request_id: &str,
        deadline: Instant,
    ) -> Result<(u16, Vec<u8>), u16> {
        if !valid_identity(request_id) {
            return Err(400);
        }
        let inventory = inventory(socket, deadline)?;
        let found = inventory.find(request_id)?;
        let state = if !inventory.complete() {
            "unknown"
        } else {
            found.map_or("absent", |(_, s)| s.state.as_str())
        };
        encode_job(
            if state == "absent" { 404 } else { 200 },
            request_id,
            found.map(|(op, _)| op),
            state,
            found.and_then(|(_, s)| (state == "exited").then_some(s.exit_code).flatten()),
        )
    }
    pub fn logs(
        &self,
        socket: &Path,
        request_id: &str,
        deadline: Instant,
    ) -> Result<(u16, Vec<u8>), u16> {
        if !valid_identity(request_id) {
            return Err(400);
        }
        let inventory = inventory(socket, deadline)?;
        let found = inventory.find(request_id)?;
        if !inventory.complete() {
            return encode_job(503, request_id, found.map(|(op, _)| op), "unknown", None);
        }
        let Some((operation, sandbox)) = found else {
            return encode_job(404, request_id, None, "absent", None);
        };
        let value = hub::call_until(
            socket,
            Request::Logs {
                handle: sandbox.handle.clone(),
                tail: Some(LOG_MAX_TAIL),
            },
            deadline,
        )
        .map_err(|_| 503u16)?;
        let logs: Logs = serde_json::from_value(value).map_err(|_| 503u16)?;
        let body = serde_json::to_vec(&JobLogs {
            request_id,
            operation,
            state: &sandbox.state,
            logs,
        })
        .map_err(|_| 503u16)?;
        if body.len() > API_SIZE_LIMIT {
            return Err(503);
        }
        Ok((200, body))
    }
    pub fn submit(
        &self,
        socket: &Path,
        bytes: &[u8],
        deadline: Instant,
    ) -> Result<(u16, Vec<u8>), u16> {
        let submission: Submission = serde_json::from_slice(bytes).map_err(|_| 400u16)?;
        let Submission {
            request_id,
            operation,
        } = submission;
        if !valid_identity(&request_id) {
            return Err(400);
        }
        // Serialize check/place/reconcile, including changed-operation retries.
        let _guard = self.submissions.lock().map_err(|_| 503u16)?;
        let before = inventory(socket, deadline)?;
        if let Some((existing, _)) = before.find(&request_id)?
            && existing != operation
        {
            return Err(409);
        }
        let found = before.find(&request_id)?;
        if let Some((_, sandbox)) = found {
            return encode_job(
                if before.complete() { 200 } else { 503 },
                &request_id,
                Some(&operation),
                if before.complete() {
                    &sandbox.state
                } else {
                    "unknown"
                },
                if before.complete() && sandbox.state == "exited" {
                    sandbox.exit_code
                } else {
                    None
                },
            );
        }
        let approved = self.operations.get(&operation).ok_or(404u16)?;
        if !before.complete() {
            return encode_job(503, &request_id, Some(&operation), "unknown", None);
        }
        let _ = hub::call_until(
            socket,
            Request::Place {
                spec: Placement {
                    fresh: false,
                    sandbox_id: format!("{}{operation}", prefix(&request_id)),
                    image: approved.image.clone(),
                    cpu_millicores: approved.cpu_millicores,
                    memory_bytes: approved.memory_bytes,
                    preferences: Vec::new(),
                    profile: None,
                    command: approved.command.clone(),
                    environment: approved.environment.clone(),
                },
            },
            deadline,
        );
        // A returned handle alone is not a state observation. Reconcile even on
        // error, including a lost placement response. Never automatically replay.
        let after = inventory(socket, deadline);
        if let Ok(after) = after
            && after.complete()
            && let Some((existing, sandbox)) = after.find(&request_id)?
        {
            if existing != operation {
                return Err(409);
            }
            return encode_job(
                200,
                &request_id,
                Some(&operation),
                &sandbox.state,
                if sandbox.state == "exited" {
                    sandbox.exit_code
                } else {
                    None
                },
            );
        }
        encode_job(503, &request_id, Some(&operation), "unknown", None)
    }
}
fn encode_job(
    status: u16,
    request_id: &str,
    operation: Option<&str>,
    state: &str,
    exit_code: Option<i64>,
) -> Result<(u16, Vec<u8>), u16> {
    serde_json::to_vec(&Job {
        request_id,
        operation,
        state,
        exit_code,
    })
    .map(|body| (status, body))
    .map_err(|_| 503)
}
fn inventory(socket: &Path, deadline: Instant) -> Result<HubInventory, u16> {
    let value = hub::call_until(socket, Request::Inventory {}, deadline).map_err(|_| 503u16)?;
    serde_json::from_value(value).map_err(|_| 503)
}

// Decode the complete known schema, then construct separate public responses.
// Additive hub fields fail closed until explicitly reviewed here.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HubInventory {
    settling: bool,
    members: Vec<HubMember>,
}
impl HubInventory {
    fn complete(&self) -> bool {
        !self.settling && self.members.iter().all(|m| m.inventory_complete)
    }
    fn find(&self, request_id: &str) -> Result<Option<(&str, &HubSandbox)>, u16> {
        let prefix = prefix(request_id);
        let mut matches = self
            .members
            .iter()
            .flat_map(|m| &m.sandboxes)
            .filter_map(|s| {
                let operation = s.sandbox_id.strip_prefix(&prefix)?;
                valid_operation_name(operation).then_some((operation, s))
            });
        let found = matches.next();
        if matches.next().is_some() {
            return Err(503);
        }
        Ok(found)
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HubMember {
    member: u64,
    #[serde(rename = "state")]
    _state: String,
    inventory_complete: bool,
    #[serde(rename = "free")]
    _free: HubBudget,
    sandboxes: Vec<HubSandbox>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HubBudget {
    #[serde(rename = "cpu_millicores")]
    _cpu: u64,
    #[serde(rename = "memory_bytes")]
    _memory: u64,
    #[serde(rename = "sandbox_slots")]
    _slots: u32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HubSandbox {
    handle: String,
    sandbox_id: String,
    state: String,
    exit_code: Option<i64>,
    #[serde(rename = "cpu_millicores")]
    _cpu: Option<u64>,
    #[serde(rename = "memory_bytes")]
    _memory: Option<u64>,
}
#[derive(Serialize)]
struct Sandboxes {
    sandboxes: Vec<Sandbox>,
    inventory_complete: bool,
}
#[derive(Serialize)]
struct Sandbox {
    member: u64,
    sandbox_id: String,
    state: String,
    exit_code: Option<i64>,
}
pub fn encode_inventory(value: serde_json::Value) -> Result<Vec<u8>, u16> {
    let inventory: HubInventory = serde_json::from_value(value).map_err(|_| 503u16)?;
    let inventory_complete = inventory.complete();
    let sandboxes = inventory
        .members
        .into_iter()
        .flat_map(|m| {
            m.sandboxes.into_iter().map(move |s| Sandbox {
                member: m.member,
                sandbox_id: s.sandbox_id,
                state: s.state,
                exit_code: s.exit_code,
            })
        })
        .collect();
    serde_json::to_vec(&Sandboxes {
        sandboxes,
        inventory_complete,
    })
    .map_err(|_| 503)
}
