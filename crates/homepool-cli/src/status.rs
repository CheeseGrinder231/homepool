//! Explicit, versioned output contract independent of core's Rust representation.

use std::fmt::Write;

use homepool_core::{ComputeBudget, MemberState, Pool, Tier};
use serde::Serialize;

/// Version of the status JSON output contract, independent of the member-file schema.
const STATUS_JSON_VERSION: u64 = 1;

#[derive(Serialize)]
struct Status {
    version: u64,
    members: Vec<MemberStatus>,
}

#[derive(Serialize)]
struct MemberStatus {
    id: u64,
    tier: &'static str,
    state: &'static str,
    capabilities: Capabilities,
}

#[derive(Serialize)]
struct Capabilities {
    compute: Option<Budget>,
    storage_bytes: u64,
}

#[derive(Serialize)]
struct Budget {
    cpu_millicores: u64,
    memory_bytes: u64,
    sandbox_slots: u32,
}

impl From<ComputeBudget> for Budget {
    fn from(value: ComputeBudget) -> Self {
        Self {
            cpu_millicores: value.cpu_millicores,
            memory_bytes: value.memory_bytes,
            sandbox_slots: value.sandbox_slots,
        }
    }
}

pub fn render(pool: &Pool, json: bool) -> Result<String, String> {
    let status = Status {
        version: STATUS_JSON_VERSION,
        members: pool
            .members()
            .map(|member| MemberStatus {
                id: member.id.0,
                tier: match member.tier {
                    Tier::Core => "core",
                    Tier::Burst => "burst",
                },
                state: match member.state {
                    MemberState::Active => "active",
                    MemberState::Draining => "draining",
                    MemberState::Unreachable => "unreachable",
                },
                capabilities: Capabilities {
                    compute: member.capabilities.compute.map(Budget::from),
                    storage_bytes: member.capabilities.storage_bytes,
                },
            })
            .collect(),
    };
    if json {
        return serde_json::to_string(&status)
            .map(|text| format!("{text}\n"))
            .map_err(|e| format!("could not encode status: {e}"));
    }
    let mut output = String::from("Declared members (from file; no live probes)\n");
    if status.members.is_empty() {
        output.push_str("No members.\n");
    }
    for member in status.members {
        write!(output, "{}  {}  {}  ", member.id, member.tier, member.state)
            .expect("write to String");
        if let Some(budget) = member.capabilities.compute {
            write!(
                output,
                "compute: {} millicores, {} memory bytes, {} sandbox slots; ",
                budget.cpu_millicores, budget.memory_bytes, budget.sandbox_slots
            )
            .expect("write to String");
        } else {
            output.push_str("compute: none; ");
        }
        writeln!(
            output,
            "storage: {} bytes",
            member.capabilities.storage_bytes
        )
        .expect("write to String");
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_json_v1_contract() {
        assert_eq!(STATUS_JSON_VERSION, 1);
        assert_eq!(
            render(&Pool::default(), true).unwrap(),
            "{\"version\":1,\"members\":[]}\n"
        );
    }
}
