#![cfg(unix)]

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use homepool_adapter_garage::Garage;
use homepool_core::{Capabilities, Member, MemberId, MemberState, Tier};
use serde_json::{Value, json};

struct Fake {
    script: PathBuf,
}

impl Fake {
    fn new(responses: &[(&str, Value)]) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let scratch = PathBuf::from(
            std::env::var_os("TMPDIR").unwrap_or_else(|| env!("CARGO_TARGET_TMPDIR").into()),
        )
        .join(format!(
            "garage-protocol-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&scratch).unwrap();
        let script = scratch.join("fake.sh");
        fs::write(
            &script,
            r#"
test "$#" = 3 && test "$1" = json-api && test "$3" = - || exit 1
printf '%s\n' "$2" >> "$0.calls"
cat >> "$0.requests"
printf '\n' >> "$0.requests"
cat "$0.$2" 2>/dev/null || { printf 'placeholder-secret' >&2; exit 1; }
"#,
        )
        .unwrap();
        for (operation, value) in responses {
            fs::write(
                scratch.join(format!("fake.sh.{operation}")),
                serde_json::to_vec(value).unwrap(),
            )
            .unwrap();
        }
        Self { script }
    }

    fn garage(&self) -> Garage {
        Garage::with_command("sh", [self.script.as_os_str().into()])
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.script.with_extension("sh.calls")).unwrap()
    }

    fn requests(&self) -> Vec<Value> {
        fs::read_to_string(self.script.with_extension("sh.requests"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.script.parent().unwrap());
    }
}

fn member(storage_bytes: u64) -> Member {
    Member {
        id: MemberId(7),
        tier: Tier::Core,
        state: MemberState::Active,
        capabilities: Capabilities {
            storage_bytes,
            compute: None,
        },
    }
}

#[test]
fn cancellation_is_sticky_and_prevents_every_operation_from_starting() {
    use homepool_adapter_garage::Error;
    use std::sync::{Arc, atomic::AtomicBool};

    let cancelled = Arc::new(AtomicBool::new(true));
    let garage =
        Garage::with_command("no-such-program", []).with_cancellation(Arc::clone(&cancelled));
    for result in [
        garage.stage_portion(&member(1), &"a".repeat(64), "zone-a"),
        garage.apply_layout(1),
        garage.create_consumer(1).map(|_| ()),
        garage.storage_usage().map(|_| ()),
    ] {
        assert_eq!(result.unwrap_err(), Error::CANCELLED);
    }
    assert!(cancelled.load(Ordering::Acquire));
}

#[test]
fn portion_is_exact_staged_and_explicitly_applied() {
    let fake = Fake::new(&[
        (
            "GetClusterStatus",
            json!({"nodes": [{"id": "a".repeat(64)}]}),
        ),
        ("UpdateClusterLayout", json!({})),
        ("ApplyClusterLayout", json!({})),
    ]);
    let garage = fake.garage();
    let node = "a".repeat(64);
    garage
        .stage_portion(&member(123_456), &node, "zone-a")
        .unwrap();
    assert_eq!(fake.calls(), "GetClusterStatus\nUpdateClusterLayout\n");
    assert_eq!(
        fake.requests()[1],
        json!({"roles": [{"id": node, "zone": "zone-a", "capacity": 123456,
        "tags": ["homepool-member-7"]}]})
    );
    garage.apply_layout(4).unwrap();
    assert_eq!(fake.requests()[2], json!({"version": 4}));
    for (portion, node, zone) in [
        (0, node.as_str(), "zone-a"),
        (1, "short", "zone-a"),
        (1, node.as_str(), " "),
    ] {
        assert!(garage.stage_portion(&member(portion), node, zone).is_err());
    }
    assert_eq!(
        fake.calls(),
        "GetClusterStatus\nUpdateClusterLayout\nApplyClusterLayout\n"
    );
}

#[test]
fn unknown_nodes_and_failed_cluster_queries_never_stage_a_role() {
    for response in [
        json!({"nodes": []}),
        json!({"nodes": [{"id": "b".repeat(64)}]}),
        json!({}),
        json!({"nodes": null}),
    ] {
        let fake = Fake::new(&[("GetClusterStatus", response)]);
        assert!(
            fake.garage()
                .stage_portion(&member(123_456), &"a".repeat(64), "zone-a")
                .is_err()
        );
        assert_eq!(fake.calls(), "GetClusterStatus\n");
    }
    let fake = Fake::new(&[]);
    assert!(
        fake.garage()
            .stage_portion(&member(1), &"a".repeat(64), "zone-a")
            .is_err()
    );
    assert_eq!(fake.calls(), "GetClusterStatus\n");
}

#[test]
fn one_deadline_covers_provisioning_and_prevents_cleanup_after_expiry() {
    use std::time::{Duration, Instant};
    let fake = Fake::new(&provisioning());
    for timeout in [Duration::ZERO, Duration::MAX] {
        assert!(fake.garage().with_operation_timeout(timeout).is_err());
    }
    let script = fs::read_to_string(&fake.script).unwrap();
    fs::write(
        &fake.script,
        script.replace("cat \"$0.$2\"", "sleep 0.3\ncat \"$0.$2\""),
    )
    .unwrap();
    let garage = fake
        .garage()
        .with_operation_timeout(Duration::from_millis(500))
        .unwrap();
    let started = Instant::now();
    let error = garage.create_consumer(9).unwrap_err();
    assert_eq!(
        error.to_string(),
        "Garage operation deadline exceeded; inspect privately before retrying"
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(fake.calls(), "CreateBucket\nCreateKey\n");
}

fn provisioning() -> Vec<(&'static str, Value)> {
    vec![
        ("CreateBucket", json!({"id": "bucket-id"})),
        (
            "CreateKey",
            json!({"accessKeyId": "placeholder-key", "secretAccessKey": "placeholder-secret"}),
        ),
        ("AllowBucketKey", json!({})),
    ]
}

#[test]
fn fresh_key_has_only_bucket_read_write_and_debug_is_redacted() {
    let fake = Fake::new(&provisioning());
    let consumer = fake.garage().create_consumer(9).unwrap();
    assert_eq!(consumer.bucket, "homepool-consumer-9");
    assert!(consumer.credentials.access_key_id() == "placeholder-key");
    assert!(consumer.credentials.secret_access_key() == "placeholder-secret");
    let debug = format!("{consumer:?}");
    assert!(!debug.contains("placeholder"));
    let requests = fake.requests();
    assert_eq!(requests[0], json!({"globalAlias": "homepool-consumer-9"}));
    assert_eq!(
        requests[1],
        json!({"name": "homepool-consumer-9", "deny": {"createBucket": true}})
    );
    assert_eq!(
        requests[2]["permissions"],
        json!({"read": true, "write": true, "owner": false})
    );
    assert_eq!(fake.calls(), "CreateBucket\nCreateKey\nAllowBucketKey\n");
}

#[test]
fn rejected_bucket_never_creates_a_key_and_failed_grant_cleans_up() {
    let fake = Fake::new(&[]);
    let error = fake.garage().create_consumer(9).unwrap_err();
    assert!(!format!("{error:?}: {error}").contains("placeholder"));
    assert_eq!(fake.calls(), "CreateBucket\n");

    let mut responses = provisioning();
    responses.retain(|(op, _)| *op != "AllowBucketKey");
    responses.extend([("DeleteKey", json!({})), ("DeleteBucket", json!({}))]);
    let fake = Fake::new(&responses);
    assert!(fake.garage().create_consumer(9).is_err());
    assert_eq!(
        fake.calls(),
        "CreateBucket\nCreateKey\nAllowBucketKey\nDeleteKey\nDeleteBucket\n"
    );
}

#[test]
fn usage_excludes_other_buckets_and_counts_multiple_aliases_once() {
    let fake = Fake::new(&[
        (
            "ListBuckets",
            json!([
                {"id": "included", "globalAliases": ["homepool-consumer-1", "homepool-consumer-2"]},
                {"id": "other", "globalAliases": ["other", "homepool-consumer-01", "homepool-consumer-"]}
            ]),
        ),
        (
            "GetBucketInfo",
            json!({"objects": 2, "bytes": 123, "unfinishedMultipartUploadBytes": 45}),
        ),
    ]);
    let usage = fake.garage().storage_usage().unwrap();
    assert_eq!(
        (
            usage.buckets,
            usage.objects,
            usage.used_bytes,
            usage.unfinished_upload_bytes
        ),
        (1, 2, 123, 45)
    );
    assert_eq!(fake.calls(), "ListBuckets\nGetBucketInfo\n");
    assert_eq!(fake.requests()[0], Value::Null);
}

#[test]
fn malformed_counters_failed_probes_and_overflows_are_errors() {
    for counter in [
        json!(-1),
        json!(null),
        json!("placeholder-secret"),
        json!(u64::MAX),
    ] {
        let fake = Fake::new(&[
            (
                "ListBuckets",
                json!([
                    {"id": "a", "globalAliases": ["homepool-consumer-1"]},
                    {"id": "b", "globalAliases": ["homepool-consumer-2"]}
                ]),
            ),
            (
                "GetBucketInfo",
                json!({"objects": 1, "bytes": counter, "unfinishedMultipartUploadBytes": 0}),
            ),
        ]);
        let error = fake.garage().storage_usage().unwrap_err();
        assert!(!format!("{error:?}: {error}").contains("placeholder"));
    }
    let fake = Fake::new(&[(
        "ListBuckets",
        json!([{"id": "a", "globalAliases": ["homepool-consumer-1"]}]),
    )]);
    assert!(fake.garage().storage_usage().is_err());
    let fake = Fake::new(&[]);
    fs::write(
        fake.script.with_extension("sh.ListBuckets"),
        "placeholder-secret",
    )
    .unwrap();
    let error = fake.garage().storage_usage().unwrap_err();
    assert!(!format!("{error:?}: {error}").contains("placeholder"));
}
