//! Opt-in Docker integration, also run explicitly by the CI test job.
mod support;
use homepool_adapter_garage::{Consumer, Garage};
use homepool_core::{Capabilities, Member, MemberId, MemberState, Tier};
use serde_json::{Value, json};
use std::{
    thread,
    time::{Duration, Instant},
};
use support::Installation;

fn s3(installation: &Installation, consumer: &Consumer, bucket: &str, action: &str) -> bool {
    installation.s3(
        consumer.credentials.access_key_id(),
        consumer.credentials.secret_access_key(),
        bucket,
        action,
    )
}

#[test]
#[ignore = "requires Docker; CI runs this explicitly with TMPDIR set"]
fn single_node_portion_scoped_consumers_and_cross_container_s3() {
    let installation = Installation::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = installation.raw("GetClusterStatus", Value::Null) {
            break status;
        }
        assert!(Instant::now() < deadline, "Garage did not become ready");
        thread::sleep(Duration::from_millis(200));
    };
    let node_id = status["nodes"][0]["id"].as_str().expect("node ID");
    let member = Member {
        id: MemberId(1),
        profiles: Default::default(),
        tier: Tier::Core,
        state: MemberState::Active,
        capabilities: Capabilities {
            storage_bytes: 1_073_741_824,
            compute: None,
        },
    };
    let garage = Garage::with_command(
        "docker",
        ["exec", "-i", &installation.container, "/garage"].map(std::ffi::OsString::from),
    );
    garage.stage_portion(&member, node_id, "zone-a").unwrap();
    garage.apply_layout(1).unwrap();
    let layout = installation.raw("GetClusterLayout", Value::Null).unwrap();
    assert_eq!(
        layout["roles"][0]["capacity"],
        member.capabilities.storage_bytes
    );
    assert_eq!(layout["roles"][0]["tags"], json!(["homepool-member-1"]));

    let first = garage.create_consumer(1).unwrap();
    let second = garage.create_consumer(2).unwrap();
    assert!(
        garage.create_consumer(1).is_err(),
        "must not issue a second key"
    );
    for consumer in [&first, &second] {
        let key = installation
            .raw(
                "GetKeyInfo",
                json!({
                    "id": consumer.credentials.access_key_id(), "showSecretKey": false
                }),
            )
            .unwrap();
        assert_eq!(key["permissions"]["createBucket"], false);
        assert_eq!(key["buckets"].as_array().unwrap().len(), 1);
        assert_eq!(
            key["buckets"][0]["permissions"],
            json!({"read": true, "write": true, "owner": false})
        );
    }
    let keys = installation.raw("ListKeys", Value::Null).unwrap();
    assert_eq!(keys.as_array().unwrap().len(), 2);
    assert!(
        s3(&installation, &first, &first.bucket, "put"),
        "first container must write"
    );
    assert!(
        s3(&installation, &first, &first.bucket, "get"),
        "another container must read"
    );
    assert!(s3(&installation, &second, &second.bucket, "put"));
    assert!(s3(&installation, &first, &second.bucket, "deny-get"));
    assert!(s3(&installation, &second, &first.bucket, "deny-get"));
    assert!(s3(&installation, &first, &second.bucket, "deny-put"));
    assert!(s3(&installation, &first, "unowned-consumer", "deny-create"));

    // A well-formed but nonexistent node must leave both active and staged layout
    // untouched. Garage itself accepts such roles, which can break later PUTs.
    let before = installation.raw("GetClusterLayout", Value::Null).unwrap();
    let unknown = if node_id == "0".repeat(64) {
        "f".repeat(64)
    } else {
        "0".repeat(64)
    };
    let error = garage
        .stage_portion(&member, &unknown, "zone-a")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Garage storage node is not known to the cluster"
    );
    assert_eq!(
        installation.raw("GetClusterLayout", Value::Null).unwrap(),
        before
    );
    for _ in 0..3 {
        assert!(
            s3(&installation, &first, &first.bucket, "put"),
            "rejected unknown node must not disrupt S3 writes"
        );
    }
    assert!(s3(&installation, &first, &first.bucket, "get"));

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let usage = garage.storage_usage().unwrap();
        if usage.objects == 2 && usage.used_bytes == 2 * 24 {
            assert_eq!(usage.buckets, 2);
            assert_eq!(usage.unfinished_upload_bytes, 0);
            break;
        }
        assert!(Instant::now() < deadline, "usage counters did not converge");
        thread::sleep(Duration::from_millis(200));
    }
}
