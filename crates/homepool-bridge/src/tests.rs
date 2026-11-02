use crate::{peer::Peer, settings, state::*};
use homepool_protocol::{Mode, Session, wire};
use serde_json::json;
use std::{fs, path::PathBuf};
struct Fixture {
    settlements: crate::settlement::Settlements,
    dir: PathBuf,
    c: Config,
    s: State,
    identity: Identity,
    invitation: String,
}
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("homepool-bridge-{}", random(16).unwrap()));
        init(&dir).unwrap();
        let c = Config {
            state_dir: dir.clone(),
            relay_url: String::new(),
            route: "test".into(),
            bridge_token_file: dir.join("token"),
            page_origin: String::new(),
            s3_endpoint: String::new(),
            bucket: String::new(),
            region: String::new(),
            credentials_file: dir.join("credentials"),
        };
        let mut s = State::load(&dir).unwrap();
        let invitation = random(16).unwrap();
        let binding = s.binding().unwrap();
        s.invitations.insert(
            invitation.clone(),
            Invitation {
                id: invitation.clone(),
                psk: random(32).unwrap(),
                token: random(32).unwrap(),
                binding,
                expiry: now() + 600,
                device: None,
            },
        );
        s.save(&dir).unwrap();
        let identity = Identity::load(&dir).unwrap();
        Self {
            settlements: crate::settlement::Settlements::default(),
            dir,
            c,
            s,
            identity,
            invitation,
        }
    }
    fn enroll(&mut self) -> (Peer, Session, String) {
        let i = &self.s.invitations[&self.invitation];
        let private = unhex(&random(32).unwrap()).unwrap();
        let public = unhex(&self.identity.public).unwrap();
        let psk = unhex(&i.psk).unwrap();
        let mut phone = Session::new(
            Mode::Enroll,
            true,
            "test",
            &private,
            Some(&public),
            Some(&psk),
        )
        .unwrap();
        let mut peer = Peer::new(&self.c, &self.s, &self.identity, i.binding).unwrap();
        let first = phone.message(false, true, &[]).unwrap();
        let second = peer
            .receive(&self.c, &mut self.s, &mut self.settlements, &first)
            .unwrap()
            .remove(0);
        phone.message(true, true, &second).unwrap();
        let third = phone.message(false, true, &[]).unwrap();
        let ack = peer
            .receive(&self.c, &mut self.s, &mut self.settlements, &third)
            .unwrap()
            .remove(0);
        let ack = wire::decode(&phone.message(true, false, &ack).unwrap()).unwrap();
        assert_eq!(ack["type"], "enrolled");
        let stored = State::load(&self.dir).unwrap();
        assert_eq!(stored.devices.len(), 1);
        assert!(stored.invitations[&self.invitation].device.is_some());
        (peer, phone, ack["id"].as_str().unwrap().into())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
#[test]
fn durable_enrollment_and_used_invitation_do_not_reopen() {
    let mut f = Fixture::new();
    let (_, _, device) = f.enroll();
    f.s = State::load(&f.dir).unwrap();
    assert_eq!(
        f.s.invitations[&f.invitation].device.as_deref(),
        Some(device.as_str())
    );
    let invite = &f.s.invitations[&f.invitation];
    let mut contender = Session::new(
        Mode::Enroll,
        true,
        "test",
        &unhex(&random(32).unwrap()).unwrap(),
        Some(&unhex(&f.identity.public).unwrap()),
        Some(&unhex(&invite.psk).unwrap()),
    )
    .unwrap();
    let mut peer = Peer::new(&f.c, &f.s, &f.identity, invite.binding).unwrap();
    assert!(
        peer.receive(
            &f.c,
            &mut f.s,
            &mut f.settlements,
            &contender.message(false, true, &[]).unwrap()
        )
        .is_err()
    );
    assert_eq!(State::load(&f.dir).unwrap().devices.len(), 1);
}
#[test]
fn paired_device_refusals_and_scoped_status() {
    let mut f = Fixture::new();
    let (mut p, mut phone, device) = f.enroll();
    let id = random(16).unwrap();
    for m in [
        json!({"type":"put-begin","id":id,"name":"image","mime":"","length":settings::MAX_FILE+1}),
        json!({"type":"enrolled","id":id,"token":random(32).unwrap()}),
    ] {
        let encrypted = phone
            .message(false, false, &wire::encode(&m).unwrap())
            .unwrap();
        let response = p
            .receive(&f.c, &mut f.s, &mut f.settlements, &encrypted)
            .unwrap()
            .remove(0);
        assert_eq!(
            wire::decode(&phone.message(true, false, &response).unwrap()).unwrap()["type"],
            "error"
        );
    }
    let entry = Entry {
        write_attempted: false,
        settled_at: None,
        committed_at: None,
        device: random(16).unwrap(),
        id: id.clone(),
        key: String::new(),
        name: "other".into(),
        mime: String::new(),
        length: 0,
        status: "pending".into(),
        hash: String::new(),
        created: now(),
    };
    f.s.journal.insert(format!("{}:{id}", entry.device), entry);
    let message = wire::encode(&json!({"type":"put-status","id":id})).unwrap();
    let response = p
        .receive(
            &f.c,
            &mut f.s,
            &mut f.settlements,
            &phone.message(false, false, &message).unwrap(),
        )
        .unwrap()
        .remove(0);
    assert_eq!(
        wire::decode(&phone.message(true, false, &response).unwrap()).unwrap()["type"],
        "unknown"
    );
    let quota_id = random(16).unwrap();
    f.s.journal.insert(
        format!("{device}:{quota_id}"),
        Entry {
            write_attempted: false,
            settled_at: None,
            committed_at: None,
            device: device.clone(),
            id: quota_id,
            key: String::new(),
            name: "prior".into(),
            mime: String::new(),
            length: settings::QUOTA,
            status: "committed".into(),
            hash: String::new(),
            created: now(),
        },
    );
    let begin = wire::encode(
        &json!({"type":"put-begin","id":random(16).unwrap(),"name":"quota","mime":"","length":1}),
    )
    .unwrap();
    let response = p
        .receive(
            &f.c,
            &mut f.s,
            &mut f.settlements,
            &phone.message(false, false, &begin).unwrap(),
        )
        .unwrap()
        .remove(0);
    assert_eq!(
        wire::decode(&phone.message(true, false, &response).unwrap()).unwrap()["type"],
        "error"
    );
    f.s.devices.remove(&device);
    assert!(
        p.receive(
            &f.c,
            &mut f.s,
            &mut f.settlements,
            &phone.message(false, false, &message).unwrap()
        )
        .is_err()
    );
}
#[test]
fn corrupt_and_missing_identity_refuse_and_private_modes_hold() {
    let f = Fixture::new();
    private(&f.dir, true).unwrap();
    private(&f.dir.join("identity.json"), false).unwrap();
    private(&f.dir.join("state.json"), false).unwrap();
    fs::write(f.dir.join("identity.json"), b"{}").unwrap();
    assert!(Identity::load(&f.dir).is_err());
    fs::remove_file(f.dir.join("identity.json")).unwrap();
    assert!(Identity::load(&f.dir).is_err());
    assert!(!f.dir.join("identity.json").exists());
}
#[test]
fn durable_quota_and_duplicate_request_reservation() {
    let mut f = Fixture::new();
    let id = random(16).unwrap();
    let device = random(16).unwrap();
    let e = Entry {
        write_attempted: false,
        settled_at: None,
        committed_at: None,
        device: device.clone(),
        id: id.clone(),
        key: String::new(),
        name: "image".into(),
        mime: String::new(),
        length: 1,
        status: "pending".into(),
        hash: String::new(),
        created: now(),
    };
    f.s.reserve(&f.dir, e.clone()).unwrap();
    assert!(f.s.reserve(&f.dir, e.clone()).is_err());
    f.s = State::load(&f.dir).unwrap();
    assert!(f.s.reserve(&f.dir, e.clone()).is_err());
    f.s.journal.values_mut().next().unwrap().length = settings::QUOTA;
    let mut next = e;
    next.id = random(16).unwrap();
    assert!(f.s.reserve(&f.dir, next).is_err());
    assert_eq!(f.s.journal.len(), 1);
}
#[test]
fn fresh_transport_is_required_for_operations_and_closed_schema_is_enforced() {
    for malformed in [vec![1, 255], vec![1, 0]] {
        let mut f = Fixture::new();
        let (mut p, mut phone, _) = f.enroll();
        let ciphertext = phone.message(false, false, &malformed).unwrap();
        assert!(
            p.receive(&f.c, &mut f.s, &mut f.settlements, &ciphertext)
                .is_err()
        );
    }
    let mut f = Fixture::new();
    let (mut p, mut phone, _) = f.enroll();
    let id = random(16).unwrap();
    let mut m = json!({"type":"put-begin","id":id,"name":"image","mime":"","length":1});
    m["key"] = json!("chosen overwrite target");
    assert!(wire::encode(&m).is_err());
    m.as_object_mut().unwrap().remove("key");
    let mut bytes = wire::encode(&m).unwrap();
    bytes.extend([0, 0]);
    let encrypted = phone.message(false, false, &bytes).unwrap();
    assert!(
        p.receive(&f.c, &mut f.s, &mut f.settlements, &encrypted)
            .is_err()
    );
}

#[test]
fn spool_is_reserved_before_credit_and_hashes_only_received_bytes() {
    use sha2::{Digest, Sha256};
    let mut f = Fixture::new();
    let (_, _, device) = f.enroll();
    let id = random(16).unwrap();
    let begin =
        json!({"type":"put-begin","id":id,"name":"../folder/photo.JPG","mime":"","length":3});
    let mut upload = crate::operations::begin(&f.c, &mut f.s, &device, &begin).unwrap();
    assert_eq!(upload.file.metadata().unwrap().len(), 3);
    let entry = &f.s.journal[&upload.journal];
    assert!(entry.key.ends_with("-photo.jpg"));
    assert_eq!(entry.name, "../folder/photo.JPG");
    wire::encode(&json!({"type":"get","id":id,"key":entry.key})).unwrap();
    assert_eq!(
        State::load(&f.dir).unwrap().journal[&upload.journal].status,
        "pending"
    );
    assert!(
        !crate::operations::chunk(
            &mut upload,
            &json!({"type":"chunk","id":id,"index":0,"data":[1,2,3]})
        )
        .unwrap()
    );
    assert!(
        crate::operations::chunk(
            &mut upload,
            &json!({"type":"put-end","id":id,"length":3,"count":1})
        )
        .unwrap()
    );
    assert_eq!(
        upload.hasher.finalize().as_slice(),
        Sha256::digest([1, 2, 3]).as_slice()
    );
}

#[test]
fn recovered_commit_gets_a_full_retention_period() {
    let mut f = Fixture::new();
    let e = Entry {
        write_attempted: false,
        settled_at: None,
        committed_at: Some(now()),
        device: random(16).unwrap(),
        id: random(16).unwrap(),
        key: String::new(),
        name: String::new(),
        mime: String::new(),
        length: 0,
        status: "committed".into(),
        hash: String::new(),
        created: now() - settings::RETENTION - 1,
    };
    f.s.journal.insert("recovered".into(), e);
    f.s.prune();
    assert!(f.s.journal.contains_key("recovered"));
    f.s.journal.get_mut("recovered").unwrap().committed_at = Some(now() - settings::RETENTION - 1);
    f.s.prune();
    assert!(f.s.journal.is_empty());
}

#[test]
fn abandoned_upload_fails_durably_and_keeps_request_history() {
    let mut f = Fixture::new();
    let (_, _, device) = f.enroll();
    let id = random(16).unwrap();
    let begin =
        json!({"type":"put-begin","id":id,"name":"partial","mime":"","length":settings::MAX_FILE});
    let upload = crate::operations::begin(&f.c, &mut f.s, &device, &begin).unwrap();
    let journal = upload.journal.clone();
    assert!(!f.s.journal[&journal].write_attempted);
    let path = crate::operations::spool(&f.c, &journal.replace(':', "-"));
    assert!(path.exists());
    crate::settlement::Settlements::abort(&f.c, &mut f.s, upload).unwrap();
    let durable = State::load(&f.dir).unwrap();
    assert_eq!(durable.journal[&journal].status, "failed");
    assert!(!path.exists());
    assert!(crate::operations::begin(&f.c, &mut f.s, &device, &begin).is_err());
    let next =
        json!({"type":"put-begin","id":random(16).unwrap(),"name":"next","mime":"","length":0});
    let next = crate::operations::begin(&f.c, &mut f.s, &random(16).unwrap(), &next).unwrap();
    crate::settlement::Settlements::abort(&f.c, &mut f.s, next).unwrap();
}
#[test]
fn reserve_durably_prunes_ten_thousand_expired_terminal_entries() {
    let mut f = Fixture::new();
    let old = now() - settings::RETENTION - 1;
    let template = Entry {
        write_attempted: true,
        settled_at: Some(old),
        committed_at: Some(old),
        device: random(16).unwrap(),
        id: String::new(),
        key: String::new(),
        name: String::new(),
        mime: String::new(),
        length: 0,
        status: "committed".into(),
        hash: String::new(),
        created: old,
    };
    for n in 0..settings::MAX_JOURNAL {
        let mut e = template.clone();
        e.id = format!("{n:032x}");
        f.s.journal.insert(format!("{}:{}", e.device, e.id), e);
    }
    f.s.save(&f.dir).unwrap();
    f.s = State::load(&f.dir).unwrap();
    assert_eq!(f.s.journal.len(), 10000);
    let mut next = template;
    next.id = random(16).unwrap();
    next.status = "pending".into();
    next.created = now();
    next.committed_at = None;
    next.settled_at = None;
    next.write_attempted = false;
    let key = f.s.reserve(&f.dir, next).unwrap();
    let durable = State::load(&f.dir).unwrap();
    assert_eq!(durable.journal.len(), 1);
    assert_eq!(durable.journal[&key].status, "pending");
}
#[test]
fn live_pruning_preserves_pending_and_full_commit_retention() {
    let mut f = Fixture::new();
    let old = now() - settings::RETENTION - 1;
    let entry = Entry {
        write_attempted: true,
        settled_at: None,
        committed_at: None,
        device: random(16).unwrap(),
        id: random(16).unwrap(),
        key: String::new(),
        name: String::new(),
        mime: String::new(),
        length: 0,
        status: "pending".into(),
        hash: String::new(),
        created: old,
    };
    f.s.journal.insert("pending".into(), entry.clone());
    let mut committed = entry.clone();
    committed.status = "committed".into();
    committed.committed_at = Some(now());
    f.s.journal.insert("committed".into(), committed);
    let mut failed = entry;
    failed.status = "failed".into();
    failed.settled_at = Some(now());
    f.s.journal.insert("failed".into(), failed);
    assert!(!f.s.prune());
    assert_eq!(f.s.journal.len(), 3);
}
#[test]
fn legacy_pending_state_is_conservatively_uncertain() {
    let mut f = Fixture::new();
    let (_, _, device) = f.enroll();
    let id = random(16).unwrap();
    let upload = crate::operations::begin(
        &f.c,
        &mut f.s,
        &device,
        &json!({"type":"put-begin","id":id,"name":"legacy","mime":"","length":0}),
    )
    .unwrap();
    let mut old = serde_json::to_value(&f.s.journal[&upload.journal]).unwrap();
    old.as_object_mut().unwrap().remove("write_attempted");
    old.as_object_mut().unwrap().remove("settled_at");
    let old: Entry = serde_json::from_value(old).unwrap();
    assert!(old.write_attempted);
    crate::settlement::Settlements::abort(&f.c, &mut f.s, upload).unwrap();
}
