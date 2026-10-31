//! Rebuild accounting without interpreting failed or partial inventory as free.
use crate::{protocol::Budget, settings::Configuration};
use homepool_adapter_docker::{Docker, Sandbox};
use homepool_core::{ComputeBudget, MemberId, MemberState, Pool, Reservation, ReservationId};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Instant};

pub struct Inventory {
    pub pool: Pool,
    pub observed: BTreeMap<MemberId, Vec<Sandbox>>,
    pub complete: BTreeMap<MemberId, bool>,
    pub settling: bool,
}

pub fn adapter(
    config: &Configuration,
    member: MemberId,
    budget: &Budget,
) -> Result<Docker, String> {
    let endpoint = config
        .docker
        .get(&member)
        .ok_or("member has no Docker endpoint")?
        .endpoint()?;
    budget.check()?;
    Docker::new(endpoint, member)
        .with_operation_timeout(budget.timing.operation)
        .map(|adapter| {
            adapter
                .with_cancellation(std::sync::Arc::clone(&budget.cancelled))
                .with_deadline(budget.deadline)
        })
        .map_err(|e| e.to_string())
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
        budget: &Budget,
        settled: bool,
    ) -> Result<Self, String> {
        let mut pool = Pool::default();
        let mut observed = BTreeMap::new();
        let mut complete = BTreeMap::new();
        let mut identities = BTreeMap::new();
        let members: Vec<_> = config.pool.members().collect();
        let inventory_end =
            Instant::now() + budget.deadline.saturating_duration_since(Instant::now()) / 2;
        for (index, declared) in members.iter().enumerate() {
            let mut member = (*declared).clone();
            member.state = MemberState::Unreachable;
            pool.add_member(member).map_err(|e| e.to_string())?;
            // Fair shares of half the remaining request budget keep later
            // healthy members observable even when earlier members stall.
            let remaining_members = u32::try_from(members.len() - index).unwrap_or(u32::MAX);
            let share = inventory_end.saturating_duration_since(Instant::now()) / remaining_members;
            let member_end = Instant::now() + share;
            let result = adapter(config, declared.id, budget).and_then(|a| {
                a.with_deadline(member_end)
                    .list()
                    .map_err(|e| e.to_string())
            });
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
            if known && settled {
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
            settling: !settled,
        })
    }

    pub fn render(&self) -> Value {
        json!({"settling": self.settling, "members": self.pool.members().map(|m| {
            let free = self.pool.free_budget(m.id).expect("known member");
            json!({"member": m.id.0, "state": match m.state { MemberState::Active => "active", MemberState::Draining => "draining", MemberState::Unreachable => "unreachable" }, "inventory_complete": self.complete[&m.id], "free": {"cpu_millicores": free.cpu_millicores, "memory_bytes": free.memory_bytes, "sandbox_slots": free.sandbox_slots}, "sandboxes": self.observed[&m.id].iter().map(|s| {
                let mut value = json!({"handle": handle(m.id, &s.container_id), "sandbox_id": s.sandbox_id, "state": s.state, "cpu_millicores": s.cpu_millicores, "memory_bytes": s.memory_bytes});
                if let Some(code) = s.exit_code { value["exit_code"] = json!(code); }
                value
            }).collect::<Vec<_>>()})
        }).collect::<Vec<_>>()})
    }
}
