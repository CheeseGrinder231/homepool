use super::*;

fn member(id: u64, cpu: u64, memory: u64, slots: u32) -> Member {
    Member {
        id: MemberId(id),
        profiles: Default::default(),
        tier: Tier::Core,
        state: MemberState::Active,
        capabilities: Capabilities {
            compute: Some(ComputeBudget {
                cpu_millicores: cpu,
                memory_bytes: memory,
                sandbox_slots: slots,
            }),
            storage_bytes: 0,
        },
    }
}

fn pool(members: impl IntoIterator<Item = Member>) -> Pool {
    let mut pool = Pool::default();
    for member in members {
        pool.add_member(member).unwrap();
    }
    pool
}

fn request(cpu: u64, memory: u64) -> SandboxRequest {
    SandboxRequest {
        cpu_millicores: cpu,
        memory_bytes: memory,
        profile: None,
        preferences: Vec::new(),
    }
}

#[test]
fn fit_requires_cpu_memory_and_a_slot() {
    assert_eq!(
        Pool::default().place(&request(0, 0)),
        Err(Error::NoCapacity)
    );
    for (cpu, memory, slots) in [(3, 8, 1), (4, 7, 1), (4, 8, 0)] {
        let pool = pool([member(1, cpu, memory, slots)]);
        assert_eq!(pool.place(&request(4, 8)), Err(Error::NoCapacity));
    }
    let pool = pool([member(1, 4, 8, 1)]);
    assert_eq!(pool.place(&request(4, 8)), Ok(MemberId(1)));
    assert_eq!(pool.reservations().count(), 0);
    assert_eq!(pool.free_budget(MemberId(1)).unwrap().sandbox_slots, 1);
}

#[test]
fn storage_only_members_cannot_run_even_empty_sandboxes() {
    let mut storage = member(1, 0, 0, 0);
    storage.capabilities = Capabilities {
        compute: None,
        storage_bytes: u64::MAX,
    };
    let pool = pool([storage]);
    assert_eq!(pool.place(&request(0, 0)), Err(Error::NoCapacity));
    assert_eq!(pool.free_budget(MemberId(1)), Ok(ComputeBudget::default()));
    assert_eq!(
        pool.member(MemberId(1)).unwrap().capabilities.storage_bytes,
        u64::MAX
    );
}

#[test]
fn preferences_precede_free_budget_and_skip_ineligible_members() {
    let mut pool = pool([member(1, 10, 10, 2), member(2, 4, 4, 2), member(3, 1, 1, 2)]);
    let mut request = request(2, 2);
    request.preferences = vec![MemberId(99), MemberId(3), MemberId(2), MemberId(2)];
    assert_eq!(pool.place(&request), Ok(MemberId(2)));
    pool.set_state(MemberId(2), MemberState::Draining).unwrap();
    assert_eq!(pool.place(&request), Ok(MemberId(1)));
    pool.set_state(MemberId(2), MemberState::Unreachable)
        .unwrap();
    assert_eq!(pool.reserve(&request).unwrap().member_id, MemberId(1));
}

#[test]
fn preference_order_and_first_duplicate_position_are_respected() {
    let pool = pool([member(1, 8, 8, 2), member(2, 8, 8, 2)]);
    let mut request = request(1, 1);
    request.preferences = vec![MemberId(2), MemberId(1), MemberId(2)];
    assert_eq!(pool.place(&request), Ok(MemberId(2)));
    request.preferences.swap(0, 1);
    assert_eq!(pool.place(&request), Ok(MemberId(1)));
}

#[test]
fn free_budget_orders_cpu_then_memory_then_smallest_id() {
    for ids in [[1, 2, 3, 4], [4, 3, 2, 1], [2, 4, 1, 3]] {
        let mut pool = pool(ids.map(|id| match id {
            1 => member(id, 7, 100, 10),
            2 => member(id, 8, 8, 1),
            _ => member(id, 8, 9, 1),
        }));
        pool.set_state(MemberId(3), MemberState::Active).unwrap();
        assert_eq!(pool.place(&request(1, 1)), Ok(MemberId(3)));
        pool.set_state(MemberId(3), MemberState::Draining).unwrap();
        assert_eq!(pool.place(&request(1, 1)), Ok(MemberId(4)));
        pool.set_state(MemberId(4), MemberState::Draining).unwrap();
        assert_eq!(pool.place(&request(1, 1)), Ok(MemberId(2)));
    }
}

#[test]
fn tiers_have_no_implicit_priority_and_usage_changes_placement() {
    let mut burst = member(2, 8, 8, 2);
    burst.tier = Tier::Burst;
    let mut pool = pool([member(1, 6, 6, 2), burst]);
    assert_eq!(pool.reserve(&request(4, 4)).unwrap().member_id, MemberId(2));
    assert_eq!(pool.place(&request(1, 1)), Ok(MemberId(1)));
}

#[test]
fn inactive_members_fail_closed_and_retain_usage_on_reactivation() {
    for state in [MemberState::Draining, MemberState::Unreachable] {
        let mut pool = pool([member(1, 4, 8, 2)]);
        let reservation = pool.reserve(&request(3, 6)).unwrap();
        let free = pool.free_budget(MemberId(1)).unwrap();
        pool.set_state(MemberId(1), state).unwrap();
        assert_eq!(pool.free_budget(MemberId(1)), Ok(ComputeBudget::default()));
        assert_eq!(pool.reserve(&request(0, 0)), Err(Error::NoCapacity));
        assert_eq!(
            pool.reservations().copied().collect::<Vec<_>>(),
            [reservation]
        );
        pool.set_state(MemberId(1), MemberState::Active).unwrap();
        assert_eq!(pool.free_budget(MemberId(1)), Ok(free));
        assert_eq!(pool.reserve(&request(2, 2)), Err(Error::NoCapacity));
        pool.set_state(MemberId(1), state).unwrap();
        assert_eq!(pool.release(reservation.id), Ok(reservation));
        assert_eq!(pool.free_budget(MemberId(1)), Ok(ComputeBudget::default()));
        assert_eq!(pool.place(&request(0, 0)), Err(Error::NoCapacity));
        pool.set_state(MemberId(1), MemberState::Active).unwrap();
        assert!(pool.reserve(&request(4, 8)).is_ok());
    }
}

#[test]
fn accounting_never_exceeds_any_budget_dimension() {
    // Exhaustively vary small budgets and requirements, filling each member and
    // releasing in reverse order. The ledger and free budget must conserve all
    // three resources, including zero-sized requests that still consume slots.
    for cpu in 0..=3 {
        for memory in 0..=3 {
            for slots in 0..=3 {
                for requested_cpu in 0..=4 {
                    for requested_memory in 0..=4 {
                        let mut pool = pool([member(1, cpu, memory, slots)]);
                        let total = pool.free_budget(MemberId(1)).unwrap();
                        let request = request(requested_cpu, requested_memory);
                        let mut held = Vec::new();
                        while let Ok(reservation) = pool.reserve(&request) {
                            held.push(reservation);
                            let free = pool.free_budget(MemberId(1)).unwrap();
                            let count = held.len() as u64;
                            assert_eq!(free.cpu_millicores + count * requested_cpu, cpu);
                            assert_eq!(free.memory_bytes + count * requested_memory, memory);
                            assert_eq!(u64::from(free.sandbox_slots) + count, u64::from(slots));
                            assert_eq!(pool.reservations().count(), held.len());
                        }
                        let free = pool.free_budget(MemberId(1)).unwrap();
                        assert_eq!(pool.reserve(&request), Err(Error::NoCapacity));
                        assert_eq!(pool.free_budget(MemberId(1)), Ok(free));
                        for reservation in held.into_iter().rev() {
                            assert_eq!(pool.release(reservation.id), Ok(reservation));
                        }
                        assert_eq!(pool.free_budget(MemberId(1)), Ok(total));
                        assert_eq!(pool.reservations().count(), 0);
                    }
                }
            }
        }
    }
}

#[test]
fn integer_limits_do_not_overflow() {
    let mut pool = pool([member(1, u64::MAX, u64::MAX, u32::MAX)]);
    let first = pool.reserve(&request(u64::MAX - 1, u64::MAX - 1)).unwrap();
    let second = pool.reserve(&request(1, 1)).unwrap();
    assert_eq!(pool.reserve(&request(1, 0)), Err(Error::NoCapacity));
    assert_eq!(pool.reserve(&request(0, 1)), Err(Error::NoCapacity));
    pool.release(first.id).unwrap();
    pool.release(second.id).unwrap();
    assert!(pool.reserve(&request(u64::MAX, u64::MAX)).is_ok());
}

#[test]
fn unknown_and_double_releases_are_errors_and_ids_are_not_reused() {
    let mut pool = pool([member(1, 4, 8, 2)]);
    let first = pool.reserve(&request(2, 4)).unwrap();
    let second = pool.reserve(&request(2, 4)).unwrap();
    assert_eq!(
        pool.release(ReservationId(99)),
        Err(Error::UnknownReservation(ReservationId(99)))
    );
    assert_eq!(pool.release(first.id), Ok(first));
    let free = pool.free_budget(MemberId(1)).unwrap();
    assert_eq!(
        pool.release(first.id),
        Err(Error::UnknownReservation(first.id))
    );
    assert_eq!(pool.free_budget(MemberId(1)), Ok(free));
    let third = pool.reserve(&request(2, 4)).unwrap();
    assert!(third.id > second.id);
    assert_eq!(
        pool.reservations().copied().collect::<Vec<_>>(),
        [second, third]
    );
}

#[test]
fn releasing_uses_the_ledger_and_only_affects_its_member() {
    let mut pool = pool([member(1, 4, 8, 2), member(2, 4, 8, 2)]);
    let first = pool.reserve(&request(2, 4)).unwrap();
    let second = pool.reserve(&request(2, 4)).unwrap();
    assert_ne!(first.member_id, second.member_id);
    let second_free = pool.free_budget(second.member_id).unwrap();
    let mut snapshot = first;
    snapshot.budget.cpu_millicores = u64::MAX;
    snapshot.member_id = second.member_id;
    assert_eq!(pool.release(snapshot.id), Ok(first));
    assert_eq!(pool.free_budget(second.member_id), Ok(second_free));
    assert_eq!(pool.free_budget(first.member_id).unwrap().cpu_millicores, 4);
}

#[test]
fn duplicate_members_and_unknown_members_do_not_change_accounting() {
    let mut pool = pool([member(1, 4, 8, 2)]);
    let reservation = pool.reserve(&request(2, 4)).unwrap();
    let free = pool.free_budget(MemberId(1)).unwrap();
    assert_eq!(
        pool.add_member(member(1, 100, 100, 10)),
        Err(Error::DuplicateMember(MemberId(1)))
    );
    assert_eq!(pool.free_budget(MemberId(1)), Ok(free));
    assert_eq!(
        pool.member(MemberId(99)),
        Err(Error::UnknownMember(MemberId(99)))
    );
    assert_eq!(
        pool.free_budget(MemberId(99)),
        Err(Error::UnknownMember(MemberId(99)))
    );
    assert_eq!(
        pool.set_state(MemberId(99), MemberState::Active),
        Err(Error::UnknownMember(MemberId(99)))
    );
    pool.add_member(member(2, 1, 1, 1)).unwrap();
    assert_eq!(
        pool.members().map(|member| member.id).collect::<Vec<_>>(),
        [MemberId(1), MemberId(2)]
    );
    assert_eq!(pool.release(reservation.id), Ok(reservation));
}

#[test]
fn reservation_id_exhaustion_does_not_consume_capacity_or_reuse_ids() {
    let mut pool = pool([member(1, 4, 8, 2)]);
    pool.last_reservation_id = u64::MAX - 1;
    let last = pool.reserve(&request(2, 4)).unwrap();
    assert_eq!(last.id, ReservationId(u64::MAX));
    let free = pool.free_budget(MemberId(1)).unwrap();
    assert_eq!(
        pool.reserve(&request(1, 1)),
        Err(Error::ReservationIdsExhausted)
    );
    assert_eq!(pool.free_budget(MemberId(1)), Ok(free));
    assert_eq!(pool.reservations().copied().collect::<Vec<_>>(), [last]);
    pool.release(last.id).unwrap();
    assert_eq!(
        pool.reserve(&request(1, 1)),
        Err(Error::ReservationIdsExhausted)
    );
}

#[test]
fn imports_restore_exact_inactive_member_and_stable_identity() {
    for state in [
        MemberState::Active,
        MemberState::Draining,
        MemberState::Unreachable,
    ] {
        let mut original = pool([member(1, 8, 16, 3), member(2, 8, 16, 3)]);
        let mut wanted = request(3, 5);
        wanted.preferences = vec![MemberId(2)];
        let first = original.reserve(&wanted).unwrap();
        let second = original.reserve(&request(1, 2)).unwrap();
        let mut restarted = pool(original.members().cloned());
        restarted.set_state(MemberId(2), state).unwrap();
        // Restore out of order; ids and exact members must survive a restart.
        restarted.import_allocation(second).unwrap();
        assert_eq!(restarted.import_allocation(first), Ok(first));
        assert_eq!(
            restarted.reservations().copied().collect::<Vec<_>>(),
            original.reservations().copied().collect::<Vec<_>>()
        );
        assert_eq!(restarted.member(MemberId(2)).unwrap().state, state);
        if state != MemberState::Active {
            assert_eq!(
                restarted.free_budget(MemberId(2)),
                Ok(ComputeBudget::default())
            );
        }
        restarted
            .set_state(MemberId(2), MemberState::Active)
            .unwrap();
        assert_eq!(
            restarted.free_budget(MemberId(2)),
            original.free_budget(MemberId(2))
        );
        let next = restarted.reserve(&request(0, 0)).unwrap();
        assert!(next.id > first.id && next.id > second.id);
        assert_eq!(restarted.release(first.id), Ok(first));
    }
}

#[test]
fn incomplete_inventory_stays_unschedulable_after_partial_import_or_release() {
    let mut pool = pool([member(1, 8, 16, 3)]);
    pool.set_state(MemberId(1), MemberState::Unreachable)
        .unwrap();
    let allocation = Reservation {
        id: ReservationId(42),
        member_id: MemberId(1),
        budget: ComputeBudget {
            cpu_millicores: 3,
            memory_bytes: 5,
            sandbox_slots: 1,
        },
    };
    pool.import_allocation(allocation).unwrap();
    let mut oversized = allocation;
    oversized.id = ReservationId(43);
    oversized.budget.cpu_millicores = 6;
    assert_eq!(pool.import_allocation(oversized), Err(Error::NoCapacity));
    assert_eq!(pool.reserve(&request(0, 0)), Err(Error::NoCapacity));
    pool.release(allocation.id).unwrap();
    assert_eq!(pool.place(&request(0, 0)), Err(Error::NoCapacity));
    assert_eq!(pool.free_budget(MemberId(1)), Ok(ComputeBudget::default()));
}

#[test]
fn invalid_imports_are_atomic_and_never_redirected() {
    let mut pool = pool([member(1, u64::MAX, u64::MAX, 2), member(2, 0, 0, 1)]);
    let allocation = Reservation {
        id: ReservationId(7),
        member_id: MemberId(1),
        budget: ComputeBudget {
            cpu_millicores: u64::MAX,
            memory_bytes: u64::MAX,
            sandbox_slots: 1,
        },
    };
    pool.import_allocation(allocation).unwrap();
    let free = pool.free_budget(MemberId(1));
    assert_eq!(
        pool.import_allocation(allocation),
        Err(Error::DuplicateReservation(allocation.id))
    );
    for (member_id, budget, error) in [
        (
            MemberId(99),
            allocation.budget,
            Error::UnknownMember(MemberId(99)),
        ),
        (MemberId(2), allocation.budget, Error::NoCapacity),
        (
            MemberId(1),
            ComputeBudget {
                cpu_millicores: 1,
                memory_bytes: 0,
                sandbox_slots: 1,
            },
            Error::NoCapacity,
        ),
        (
            MemberId(1),
            ComputeBudget {
                cpu_millicores: 0,
                memory_bytes: 1,
                sandbox_slots: 1,
            },
            Error::NoCapacity,
        ),
        (
            MemberId(1),
            ComputeBudget::default(),
            Error::InvalidAllocation,
        ),
        (
            MemberId(1),
            ComputeBudget {
                sandbox_slots: 2,
                ..ComputeBudget::default()
            },
            Error::InvalidAllocation,
        ),
    ] {
        assert_eq!(
            pool.import_allocation(Reservation {
                id: ReservationId(u64::MAX),
                member_id,
                budget
            }),
            Err(error)
        );
        assert_eq!(pool.free_budget(MemberId(1)), free);
        assert_eq!(
            pool.reservations().copied().collect::<Vec<_>>(),
            [allocation]
        );
    }
    let next = pool.reserve(&request(0, 0)).unwrap();
    assert_eq!(next.id, ReservationId(8));
    // No slots remain on the exact member, despite capacity on another member.
    assert_eq!(
        pool.import_allocation(Reservation {
            id: ReservationId(9),
            member_id: next.member_id,
            budget: next.budget
        }),
        Err(Error::NoCapacity)
    );
    let mut storage = member(3, 0, 0, 0);
    storage.capabilities.compute = None;
    pool.add_member(storage).unwrap();
    assert_eq!(
        pool.import_allocation(Reservation {
            id: ReservationId(9),
            member_id: MemberId(3),
            budget: next.budget
        }),
        Err(Error::NoCapacity)
    );
}

#[test]
fn importing_the_last_id_preserves_accounting_on_exhaustion() {
    let mut pool = pool([member(1, 4, 8, 2)]);
    let allocation = Reservation {
        id: ReservationId(u64::MAX),
        member_id: MemberId(1),
        budget: ComputeBudget {
            cpu_millicores: 1,
            memory_bytes: 1,
            sandbox_slots: 1,
        },
    };
    pool.import_allocation(allocation).unwrap();
    let free = pool.free_budget(MemberId(1));
    assert_eq!(
        pool.reserve(&request(0, 0)),
        Err(Error::ReservationIdsExhausted)
    );
    assert_eq!(pool.free_budget(MemberId(1)), free);
    pool.release(allocation.id).unwrap();
    assert_eq!(
        pool.reserve(&request(0, 0)),
        Err(Error::ReservationIdsExhausted)
    );
}

#[test]
fn profiles_filter_eligibility_without_overriding_capacity_or_state() {
    let mut offered = member(2, 4, 8, 1);
    offered.profiles.insert("runner".into());
    let mut pool = pool([member(1, 20, 40, 3), offered]);
    let mut request = request(4, 8);
    request.profile = Some("missing".into());
    assert_eq!(pool.place(&request), Err(Error::NoCapacity));
    request.profile = Some("runner".into());
    request.preferences = vec![MemberId(1)];
    assert_eq!(pool.place(&request), Ok(MemberId(2)));
    let allocation = pool.reserve(&request).unwrap();
    assert_eq!(pool.place(&request), Err(Error::NoCapacity));
    pool.release(allocation.id).unwrap();
    for state in [MemberState::Draining, MemberState::Unreachable] {
        pool.set_state(MemberId(2), state).unwrap();
        assert_eq!(pool.place(&request), Err(Error::NoCapacity));
    }
    request.profile = None;
    assert_eq!(pool.place(&request), Ok(MemberId(1)));
}
