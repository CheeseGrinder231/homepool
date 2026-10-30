use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use homepool_core::MemberId;
use serde::Deserialize;

use crate::{
    command,
    endpoint::Endpoint,
    error::Error,
    limits::Limits,
    model::{EngineCapacity, Sandbox, SandboxSpec},
    settings::{
        CAPACITY_FORMAT, INSPECT_FORMAT, MANAGED_LABEL, MEMBER_LABEL, MILLICORES_PER_CPU,
        OPERATION_TIMEOUT, SANDBOX_LABEL, STOP_SECONDS,
    },
    spec,
};

/// An explicit engine endpoint and the member whose sandboxes it manages.
#[derive(Clone, Debug)]
pub struct Docker {
    endpoint: Endpoint,
    member: MemberId,
    operation_timeout: Duration,
}

#[derive(Deserialize)]
pub(crate) struct Observation {
    pub(crate) id: String,
    pub(crate) labels: Option<BTreeMap<String, String>>,
    pub(crate) state: String,
    pub(crate) limits: Limits,
}

impl Docker {
    pub fn new(endpoint: Endpoint, member: MemberId) -> Self {
        Self {
            endpoint,
            member,
            operation_timeout: OPERATION_TIMEOUT,
        }
    }

    /// Sets the end-to-end deadline for each public operation. All commands,
    /// ownership checks, inventory reads, and cleanup share this time budget.
    /// Zero and durations that cannot form a deadline are rejected.
    pub fn with_operation_timeout(mut self, timeout: Duration) -> Result<Self, Error> {
        if timeout.is_zero() || Instant::now().checked_add(timeout).is_none() {
            return Err(Error::InvalidInput("operation timeout"));
        }
        self.operation_timeout = timeout;
        Ok(self)
    }

    fn deadline(&self) -> Result<Instant, Error> {
        Instant::now()
            .checked_add(self.operation_timeout)
            .ok_or(Error::InvalidInput("operation timeout"))
    }

    /// Lists running and stopped homepool sandboxes belonging to this member.
    /// Engine or concurrent removal errors are returned, never treated as empty.
    pub fn list(&self) -> Result<Vec<Sandbox>, Error> {
        let deadline = self.deadline()?;
        let output = self.run(
            deadline,
            "list",
            vec![
                "container".into(),
                "ls".into(),
                "--all".into(),
                "--quiet".into(),
                "--no-trunc".into(),
                "--filter".into(),
                format!("label={MANAGED_LABEL}=true"),
                "--filter".into(),
                format!("label={MEMBER_LABEL}={}", self.member.0),
                "--filter".into(),
                format!("label={SANDBOX_LABEL}"),
            ],
            &[],
        )?;
        let text = std::str::from_utf8(&output).map_err(|_| Error::Protocol("list"))?;
        text.lines().map(|id| self.owned(deadline, id)).collect()
    }

    /// Creates then starts a sandbox. On a start failure, attempts to remove the
    /// created container. No environment values enter arguments or labels.
    /// The sandbox identity is descriptive; it is not an idempotency key.
    pub fn start(&self, spec: &SandboxSpec) -> Result<Sandbox, Error> {
        let deadline = self.deadline()?;
        let args = spec::create_args(self.member, spec)?;
        let output = self.run(deadline, "create", args, &spec.environment.stdin_bytes())?;
        let id = std::str::from_utf8(&output)
            .map_err(|_| Error::Protocol("create"))?
            .trim();
        validate_id(id).map_err(|_| Error::Protocol("create"))?;
        if self
            .run(
                deadline,
                "start",
                vec!["container".into(), "start".into(), id.into()],
                &[],
            )
            .is_err()
        {
            let cleanup_failed = self
                .run(
                    deadline,
                    "cleanup",
                    vec!["container".into(), "rm".into(), "--force".into(), id.into()],
                    &[],
                )
                .is_err();
            return Err(Error::StartFailed {
                container_id: id.into(),
                cleanup_failed,
            });
        }
        self.owned(deadline, id)
    }

    /// Stops only a container carrying this member's homepool labels, allowing
    /// ten seconds for graceful termination before Docker kills it.
    pub fn stop(&self, container_id: &str) -> Result<(), Error> {
        let deadline = self.deadline()?;
        self.owned(deadline, container_id)?;
        self.run(
            deadline,
            "stop",
            vec![
                "container".into(),
                "stop".into(),
                "--time".into(),
                STOP_SECONDS.into(),
                container_id.into(),
            ],
            &[],
        )?;
        Ok(())
    }

    /// Removes a stopped managed container. Does not remove volumes or force a
    /// running container to stop; call `stop` first.
    pub fn remove(&self, container_id: &str) -> Result<(), Error> {
        let deadline = self.deadline()?;
        self.owned(deadline, container_id)?;
        self.run(
            deadline,
            "remove",
            vec!["container".into(), "rm".into(), container_id.into()],
            &[],
        )?;
        Ok(())
    }

    /// Reads engine totals. Free resources and sandbox slots are caller policy.
    pub fn capacity(&self) -> Result<EngineCapacity, Error> {
        let deadline = self.deadline()?;
        #[derive(Deserialize)]
        struct Info {
            cpus: u64,
            memory_bytes: u64,
        }
        let output = self.run(
            deadline,
            "capacity",
            vec!["info".into(), "--format".into(), CAPACITY_FORMAT.into()],
            &[],
        )?;
        let info: Info =
            serde_json::from_slice(&output).map_err(|_| Error::Protocol("capacity"))?;
        let cpu_millicores = info
            .cpus
            .checked_mul(MILLICORES_PER_CPU)
            .ok_or(Error::Protocol("capacity"))?;
        Ok(EngineCapacity {
            cpu_millicores,
            memory_bytes: info.memory_bytes,
        })
    }

    fn run(
        &self,
        deadline: Instant,
        operation: &'static str,
        args: Vec<String>,
        input: &[u8],
    ) -> Result<Vec<u8>, Error> {
        command::run(&self.endpoint, operation, &args, input, deadline)
    }

    fn owned(&self, deadline: Instant, id: &str) -> Result<Sandbox, Error> {
        validate_id(id)?;
        let output = self.run(
            deadline,
            "inspect",
            vec![
                "container".into(),
                "inspect".into(),
                "--format".into(),
                INSPECT_FORMAT.into(),
                id.into(),
            ],
            &[],
        )?;
        let observed: Observation =
            serde_json::from_slice(&output).map_err(|_| Error::Protocol("inspect"))?;
        owned_observation(self.member, id, observed)
    }
}

pub(crate) fn validate_id(id: &str) -> Result<(), Error> {
    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::InvalidInput("container id"));
    }
    Ok(())
}

pub(crate) fn owned_observation(
    member: MemberId,
    id: &str,
    observed: Observation,
) -> Result<Sandbox, Error> {
    if observed.id != id {
        return Err(Error::Protocol("inspect"));
    }
    let labels = observed.labels.unwrap_or_default();
    if labels.get(MANAGED_LABEL).map(String::as_str) != Some("true")
        || labels.get(MEMBER_LABEL) != Some(&member.0.to_string())
    {
        return Err(Error::NotManaged);
    }
    let sandbox_id = labels
        .get(SANDBOX_LABEL)
        .filter(|value| !value.is_empty())
        .ok_or(Error::NotManaged)?
        .clone();
    Ok(Sandbox {
        container_id: observed.id,
        sandbox_id,
        state: observed.state,
        cpu_millicores: observed.limits.cpu_millicores()?,
        memory_bytes: (observed.limits.memory_bytes > 0).then_some(observed.limits.memory_bytes),
    })
}
