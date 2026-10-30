//! Rebuild accounting without interpreting failed or partial inventory as free.
use crate::settings::Configuration;
use homepool_adapter_docker::{Docker, Sandbox};
use homepool_core::{ComputeBudget, MemberId, MemberState, Pool, Reservation, ReservationId};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::AtomicBool},
};

pub struct Inventory {
    pub pool: Pool,
    pub observed: BTreeMap<MemberId, Vec<Sandbox>>,
    pub complete: BTreeMap<MemberId, bool>,
}

pub fn adapter(
    config: &Configuration,
    member: MemberId,
    cancelled: &Arc<AtomicBool>,
) -> Result<Docker, String> {
    let endpoint = config
        .docker
        .get(&member)
        .ok_or("member has no Docker endpoint")?
        .endpoint()?;
    Ok(Docker::new(endpoint, member).with_cancellation(Arc::clone(cancelled)))
}

// Deterministic FNV-1a on the unambiguous pair, with an explicit collision check
// at import. The external handle uses the full pair, never this numeric hash.
fn reservation(member: MemberId, container: &str) -> ReservationId {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in member.0.to_be_bytes().into_iter().chain(container.bytes()) {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    ReservationId(hash % (u64::MAX - 1) + 1)
}

pub fn handle(member: MemberId, container: &str) -> String {
    format!("{}:{container}", member.0)
}

impl Inventory {
    pub fn rebuild(
        config: &Configuration,
        previous: BTreeMap<MemberId, Vec<Sandbox>>,
        cancelled: &Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let mut pool = Pool::default();
        let mut observed = BTreeMap::new();
        let mut complete = BTreeMap::new();
        let mut identities = BTreeMap::new();
        for declared in config.pool.members() {
            let mut member = declared.clone();
            member.state = MemberState::Unreachable;
            pool.add_member(member).map_err(|e| e.to_string())?;
            let result = adapter(config, declared.id, cancelled)
                .and_then(|a| a.list().map_err(|e| e.to_string()));
            let (sandboxes, mut known) = match result {
                Ok(sandboxes) => (sandboxes, true),
                Err(_) => (
                    previous.get(&declared.id).cloned().unwrap_or_default(),
                    false,
                ),
            };
            for sandbox in &sandboxes {
                let id = reservation(declared.id, &sandbox.container_id);
                let key = handle(declared.id, &sandbox.container_id);
                if identities
                    .insert(id, key.clone())
                    .is_some_and(|old| old != key)
                {
                    known = false;
                    continue;
                }
                let (Some(cpu), Some(memory)) = (sandbox.cpu_millicores, sandbox.memory_bytes)
                else {
                    known = false;
                    continue;
                };
                if pool
                    .import_allocation(Reservation {
                        id,
                        member_id: declared.id,
                        budget: ComputeBudget {
                            cpu_millicores: cpu,
                            memory_bytes: memory,
                            sandbox_slots: 1,
                        },
                    })
                    .is_err()
                {
                    known = false;
                }
            }
            // A rejected import retains every observed handle but closes admission.
            // The next complete rebuild is the only way to activate this member.
            if known {
                pool.set_state(declared.id, declared.state)
                    .map_err(|e| e.to_string())?;
            }
            complete.insert(declared.id, known);
            observed.insert(declared.id, sandboxes);
        }
        Ok(Self {
            pool,
            observed,
            complete,
        })
    }

    pub fn render(&self) -> Value {
        json!({"members": self.pool.members().map(|m| {
            let free = self.pool.free_budget(m.id).expect("known member");
            json!({"member": m.id.0, "state": match m.state { MemberState::Active => "active", MemberState::Draining => "draining", MemberState::Unreachable => "unreachable" }, "inventory_complete": self.complete[&m.id], "free": {"cpu_millicores": free.cpu_millicores, "memory_bytes": free.memory_bytes, "sandbox_slots": free.sandbox_slots}, "sandboxes": self.observed[&m.id].iter().map(|s| json!({"handle": handle(m.id, &s.container_id), "sandbox_id": s.sandbox_id, "state": s.state, "cpu_millicores": s.cpu_millicores, "memory_bytes": s.memory_bytes})).collect::<Vec<_>>()})
        }).collect::<Vec<_>>()})
    }
}
