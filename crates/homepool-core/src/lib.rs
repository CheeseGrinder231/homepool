//! Pure pool membership, placement, and reservation accounting. No I/O is performed.

use std::collections::BTreeMap;
use std::fmt;

/// An opaque member identifier; ascending numeric order breaks placement ties.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct MemberId(pub u64);

/// Availability expectations, independent of the member's current state.
/// Neither tier receives implicit placement priority; use request preferences.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tier {
    /// Expected to remain on.
    Core,
    /// May come and go.
    Burst,
}

/// Whether a member can accept new work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemberState {
    /// Accepts work within its free compute budget.
    Active,
    /// Finishes existing work but accepts no new work.
    Draining,
    /// Usage cannot be observed; no capacity is considered free.
    Unreachable,
}

/// Integer compute capacity, used for both total and free budgets.
/// One CPU core is 1,000 millicores; memory is measured in bytes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ComputeBudget {
    /// CPU capacity in millicores.
    pub cpu_millicores: u64,
    /// Memory capacity in bytes.
    pub memory_bytes: u64,
    /// Maximum number of simultaneous sandboxes.
    pub sandbox_slots: u32,
}

/// Resources a member contributes. Storage is descriptive, not sandbox capacity.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Capabilities {
    /// `None` means this member contributes no compute.
    pub compute: Option<ComputeBudget>,
    /// Shared storage portion in bytes; zero means no storage contribution.
    pub storage_bytes: u64,
}

/// A member's declared contribution. Once added, only its state can change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Member {
    /// Unique identity within the pool.
    pub id: MemberId,
    /// Expected availability.
    pub tier: Tier,
    /// Current admission state.
    pub state: MemberState,
    /// Declared resource limits.
    pub capabilities: Capabilities,
}

/// A sandbox's integer resource requirements. Every request consumes one slot,
/// including requests for zero CPU or memory.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SandboxRequest {
    /// Required CPU in millicores.
    pub cpu_millicores: u64,
    /// Required memory in bytes.
    pub memory_bytes: u64,
    /// Preferred member ids, highest priority first. Empty means no preference.
    /// Unlisted members follow listed ones. Missing or ineligible ids are skipped;
    /// duplicate ids use their first position. Preferences never override fit.
    pub preferences: Vec<MemberId>,
}

/// Pool-local reservation identity, allocated monotonically and never reused.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ReservationId(pub u64);

/// A snapshot of a reservation. The pool retains authoritative accounting;
/// changing a returned snapshot cannot change its allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reservation {
    /// Identity to pass to [`Pool::release`].
    pub id: ReservationId,
    /// Member holding this allocation.
    pub member_id: MemberId,
    /// Allocated CPU, memory, and exactly one sandbox slot.
    pub budget: ComputeBudget,
}

/// Failures from pool operations. Failed operations leave accounting unchanged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// Adding a member would replace an existing identity.
    DuplicateMember(MemberId),
    /// The requested member is absent.
    UnknownMember(MemberId),
    /// No active compute member fits all requested resources and one slot.
    NoCapacity,
    /// The reservation is absent or has already been released.
    UnknownReservation(ReservationId),
    /// All pool-local reservation identifiers have been issued.
    ReservationIdsExhausted,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateMember(id) => write!(f, "duplicate member {}", id.0),
            Self::UnknownMember(id) => write!(f, "unknown member {}", id.0),
            Self::NoCapacity => f.write_str("no member has enough free compute capacity"),
            Self::UnknownReservation(id) => write!(f, "unknown reservation {}", id.0),
            Self::ReservationIdsExhausted => f.write_str("reservation identifiers exhausted"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Debug)]
struct Entry {
    member: Member,
    used: ComputeBudget,
}

impl Entry {
    fn free_budget(&self) -> ComputeBudget {
        let Some(total) = self.member.capabilities.compute else {
            return ComputeBudget::default();
        };
        if self.member.state != MemberState::Active {
            return ComputeBudget::default();
        }
        ComputeBudget {
            cpu_millicores: total.cpu_millicores - self.used.cpu_millicores,
            memory_bytes: total.memory_bytes - self.used.memory_bytes,
            sandbox_slots: total.sandbox_slots - self.used.sandbox_slots,
        }
    }
}

/// In-memory membership and accounting, initially empty. Budgets describe capacity
/// dedicated to this pool; callers must reserve existing work before admitting more.
/// Members and reservations can only be read through shared references or snapshots.
#[derive(Debug, Default)]
pub struct Pool {
    members: BTreeMap<MemberId, Entry>,
    reservations: BTreeMap<ReservationId, Reservation>,
    last_reservation_id: u64,
}

impl Pool {
    /// Adds a member with no reservations. Duplicate ids are rejected, preserving
    /// both the existing member and its accounting.
    pub fn add_member(&mut self, member: Member) -> Result<(), Error> {
        if self.members.contains_key(&member.id) {
            return Err(Error::DuplicateMember(member.id));
        }
        self.members.insert(
            member.id,
            Entry {
                member,
                used: ComputeBudget::default(),
            },
        );
        Ok(())
    }

    /// Iterates over members in ascending member-id order.
    pub fn members(&self) -> impl Iterator<Item = &Member> {
        self.members.values().map(|entry| &entry.member)
    }

    /// Reads a member, or reports an unknown identity.
    pub fn member(&self, id: MemberId) -> Result<&Member, Error> {
        self.members
            .get(&id)
            .map(|entry| &entry.member)
            .ok_or(Error::UnknownMember(id))
    }

    /// Changes admission state without discarding reservations. Returning to
    /// active retains all unreleased usage; state changes never infer completion.
    pub fn set_state(&mut self, id: MemberId, state: MemberState) -> Result<(), Error> {
        let entry = self.members.get_mut(&id).ok_or(Error::UnknownMember(id))?;
        entry.member.state = state;
        Ok(())
    }

    /// Returns schedulable capacity. Draining, unreachable, and non-compute
    /// members report zero in every dimension, even after a release.
    pub fn free_budget(&self, id: MemberId) -> Result<ComputeBudget, Error> {
        self.members
            .get(&id)
            .map(Entry::free_budget)
            .ok_or(Error::UnknownMember(id))
    }

    /// Chooses an eligible member without reserving capacity. Preferences rank
    /// first. Otherwise, "most free budget" means lexicographically greatest free
    /// CPU, then free memory, measured before allocation. Slots gate eligibility
    /// only. The final tie-break is the smallest member id, regardless of insertion
    /// order. Tier supplies no implicit preference. Use [`Self::reserve`] to
    /// choose and account for an allocation in one operation.
    pub fn place(&self, request: &SandboxRequest) -> Result<MemberId, Error> {
        self.members
            .values()
            .filter_map(|entry| {
                let free = entry.free_budget();
                if free.sandbox_slots == 0
                    || free.cpu_millicores < request.cpu_millicores
                    || free.memory_bytes < request.memory_bytes
                {
                    return None;
                }
                let rank = request
                    .preferences
                    .iter()
                    .position(|id| *id == entry.member.id)
                    .unwrap_or(usize::MAX);
                Some((
                    rank,
                    std::cmp::Reverse(free.cpu_millicores),
                    std::cmp::Reverse(free.memory_bytes),
                    entry.member.id,
                ))
            })
            .min()
            .map(|(_, _, _, id)| id)
            .ok_or(Error::NoCapacity)
    }

    /// Chooses a member and reserves CPU, memory, and one sandbox slot atomically.
    /// Returns `NoCapacity` if none fits. Integer limits cannot overcommit or wrap;
    /// exhausting reservation ids returns an error without consuming capacity.
    pub fn reserve(&mut self, request: &SandboxRequest) -> Result<Reservation, Error> {
        let member_id = self.place(request)?;
        let next_id = self
            .last_reservation_id
            .checked_add(1)
            .ok_or(Error::ReservationIdsExhausted)?;
        let reservation = Reservation {
            id: ReservationId(next_id),
            member_id,
            budget: ComputeBudget {
                cpu_millicores: request.cpu_millicores,
                memory_bytes: request.memory_bytes,
                sandbox_slots: 1,
            },
        };
        // Placement proved these additions fit the fixed member budget.
        let used = &mut self
            .members
            .get_mut(&member_id)
            .expect("placed member")
            .used;
        used.cpu_millicores += reservation.budget.cpu_millicores;
        used.memory_bytes += reservation.budget.memory_bytes;
        used.sandbox_slots += 1;
        self.reservations.insert(reservation.id, reservation);
        self.last_reservation_id = next_id;
        Ok(reservation)
    }

    /// Iterates over outstanding reservations in ascending reservation-id order.
    pub fn reservations(&self) -> impl Iterator<Item = &Reservation> {
        self.reservations.values()
    }

    /// Releases the recorded allocation, including for inactive members. Unknown
    /// or already-released ids return an error. This explicit operation is the
    /// caller's assertion that the allocation has ended; no state change releases
    /// work automatically or treats unknown usage as free.
    pub fn release(&mut self, id: ReservationId) -> Result<Reservation, Error> {
        let reservation = self
            .reservations
            .remove(&id)
            .ok_or(Error::UnknownReservation(id))?;
        let used = &mut self
            .members
            .get_mut(&reservation.member_id)
            .expect("reserved member")
            .used;
        used.cpu_millicores -= reservation.budget.cpu_millicores;
        used.memory_bytes -= reservation.budget.memory_bytes;
        used.sandbox_slots -= 1;
        Ok(reservation)
    }
}

#[cfg(test)]
mod tests;
