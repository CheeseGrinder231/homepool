use homepool_protocol::{Mode, Session, wire};
use serde_json::{Value, json};
fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}
#[test]
fn community_vectors() {
    let vectors: Vec<Value> =
        serde_json::from_str(include_str!("../../../protocol/cacophony.json")).unwrap();
    for v in vectors
        .iter()
        .filter(|v| !v["protocol_name"].as_str().unwrap().contains("Noise_XK_"))
    {
        let build = |init: bool| {
            let prefix = if init { "init" } else { "resp" };
            let private = unhex(v[format!("{prefix}_static")].as_str().unwrap());
            let ephemeral = unhex(v[format!("{prefix}_ephemeral")].as_str().unwrap());
            let prologue = unhex(v[format!("{prefix}_prologue")].as_str().unwrap());
            let mut b = snow::Builder::new(v["protocol_name"].as_str().unwrap().parse().unwrap())
                .local_private_key(&private)
                .unwrap()
                .fixed_ephemeral_key_for_testing_only(&ephemeral)
                .prologue(&prologue)
                .unwrap();
            let remote = unhex(v["init_remote_static"].as_str().unwrap());
            if init {
                b = b.remote_public_key(&remote).unwrap();
            }
            let psk = v["init_psks"][0].as_str().map(unhex);
            let psk_array: Option<[u8; 32]> = psk.map(|p| p.try_into().unwrap());
            if let Some(ref p) = psk_array {
                b = b.psk(3, p).unwrap();
            }
            if init {
                b.build_initiator().unwrap()
            } else {
                b.build_responder().unwrap()
            }
        };
        let mut i = build(true);
        let mut r = build(false);
        let count = if v["protocol_name"].as_str().unwrap().contains("psk") {
            3
        } else {
            2
        };
        let mut out = [0; 65535];
        for (n, m) in v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .take(count)
            .enumerate()
        {
            let (s, d) = if n % 2 == 0 {
                (&mut i, &mut r)
            } else {
                (&mut r, &mut i)
            };
            let payload = unhex(m["payload"].as_str().unwrap());
            let expected = unhex(m["ciphertext"].as_str().unwrap());
            let len = s.write_message(&payload, &mut out).unwrap();
            assert_eq!(&out[..len], expected);
            let len = d.read_message(&expected, &mut out).unwrap();
            assert_eq!(&out[..len], payload);
        }
        assert_eq!(
            i.get_handshake_hash(),
            unhex(v["handshake_hash"].as_str().unwrap())
        );
        let mut i = i.into_transport_mode().unwrap();
        let mut r = r.into_transport_mode().unwrap();
        for (n, m) in v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .skip(count)
        {
            let (s, d) = if n % 2 == 0 {
                (&mut i, &mut r)
            } else {
                (&mut r, &mut i)
            };
            let payload = unhex(m["payload"].as_str().unwrap());
            let expected = unhex(m["ciphertext"].as_str().unwrap());
            let len = s.write_message(&payload, &mut out).unwrap();
            assert_eq!(&out[..len], expected);
            let len = d.read_message(&expected, &mut out).unwrap();
            assert_eq!(&out[..len], payload);
        }
    }
}
fn pair(wrong_key: bool, wrong_psk: bool) -> (Session, Session) {
    let b = snow::Builder::new("Noise_IK_25519_AESGCM_SHA256".parse().unwrap());
    let a = b.generate_keypair().unwrap();
    let key = b.generate_keypair().unwrap();
    let bad = b.generate_keypair().unwrap();
    (
        Session::new(
            Mode::Enroll,
            true,
            "test",
            &a.private,
            Some(if wrong_key { &bad.public } else { &key.public }),
            Some(&[0; 32]),
        )
        .unwrap(),
        Session::new(
            Mode::Enroll,
            false,
            "test",
            &key.private,
            None,
            Some(if wrong_psk { &[1; 32] } else { &[0; 32] }),
        )
        .unwrap(),
    )
}
#[test]
fn failures_close_sessions() {
    let (mut i, mut r) = pair(true, false);
    assert!(
        r.message(true, true, &i.message(false, true, &[]).unwrap())
            .is_err()
    );
    let (mut i, mut r) = pair(false, true);
    r.message(true, true, &i.message(false, true, &[]).unwrap())
        .unwrap();
    i.message(true, true, &r.message(false, true, &[]).unwrap())
        .unwrap();
    assert!(
        r.message(true, true, &i.message(false, true, &[]).unwrap())
            .is_err()
    );
    for fault in 0..3 {
        let (mut i, mut r) = pair(false, false);
        r.message(true, true, &i.message(false, true, &[]).unwrap())
            .unwrap();
        i.message(true, true, &r.message(false, true, &[]).unwrap())
            .unwrap();
        r.message(true, true, &i.message(false, true, &[]).unwrap())
            .unwrap();
        let a = i.message(false, false, &vec![0; 65519]).unwrap();
        assert_eq!(a.len(), 65535);
        let b = i.message(false, false, b"next").unwrap();
        let mut bad = if fault == 0 { a.clone() } else { b };
        if fault == 0 {
            bad[0] ^= 1;
        }
        assert!(r.message(true, false, &bad).is_err());
        assert!(r.message(true, false, &a).is_err());
        assert!(i.message(false, false, &vec![0; 65520]).is_err());
    }
}
#[test]
fn transfer_end_and_schema_refusals() {
    let id = "01".repeat(16);
    let mut t = wire::Transfer::new(id.clone(), 3).unwrap();
    let m = json!({"type":"chunk","id":id,"index":0,"data":[1,2,3]});
    assert!(!t.accept(&m).unwrap());
    assert!(
        t.accept(&json!({"type":"put-end","id":id,"length":3,"count":1}))
            .unwrap()
    );
    assert!(t.accept(&m).is_err());
    let mut t = wire::Transfer::new(id.clone(), 3).unwrap();
    assert!(
        t.accept(&json!({"type":"get-end","id":id,"length":3,"count":0}))
            .is_err()
    );
    assert!(t.accept(&m).is_err());
    for m in [
        json!({"type":"delete"}),
        json!({"type":"list","id":id,"limit":101,"cursor":""}),
        json!({"type":"get","id":id,"key":"../private"}),
        json!({"type":"put-begin","id":id,"name":"x".repeat(256),"mime":"image/jpeg","length":1}),
    ] {
        assert!(wire::encode(&m).is_err());
    }
    for b in [vec![1, 255], vec![1, 1], vec![0; 65520]] {
        assert!(wire::decode(&b).is_err());
    }
    assert!(wire::envelope(1, 1, b"content").is_err());
    assert!(wire::read_admission(&[1, 2]).is_err());
}

#[test]
fn shared_relay_and_unicode_vectors() {
    let cases: Value =
        serde_json::from_str(include_str!("../../../protocol/relay-vectors.json")).unwrap();
    for v in cases["envelopes"].as_array().unwrap() {
        let b = unhex(v["hex"].as_str().unwrap());
        let (k, s, d) = wire::unwrap(&b).unwrap();
        assert_eq!(s, v["session"].as_u64().unwrap() as u32);
        assert_eq!(d, unhex(v["data"].as_str().unwrap()));
        assert_eq!(wire::envelope(k, s, d).unwrap(), b);
    }
    for b in cases["decodeRefuse"].as_array().unwrap() {
        assert!(wire::decode(&unhex(b.as_str().unwrap())).is_err());
    }
    for m in cases["unicode"].as_array().unwrap() {
        assert_eq!(wire::decode(&wire::encode(m).unwrap()).unwrap(), *m);
    }
    for m in cases["refuse"].as_array().unwrap() {
        assert!(wire::encode(m).is_err());
    }
    for b in cases["invalid"].as_array().unwrap() {
        assert!(wire::unwrap(&unhex(b.as_str().unwrap())).is_err());
    }
}

#[test]
fn live_transfer_order_and_count() {
    let id = "01".repeat(16);
    for fault in 0..3 {
        let mut t = wire::Transfer::new(id.clone(), 2).unwrap();
        t.accept(&json!({"type":"chunk","id":id,"index":0,"data":[1]}))
            .unwrap();
        if fault == 2 {
            t.accept(&json!({"type":"chunk","id":id,"index":1,"data":[2]}))
                .unwrap();
            assert!(
                t.accept(&json!({"type":"get-end","id":id,"length":2,"count":1}))
                    .is_err()
            );
        } else {
            assert!(
                t.accept(
                    &json!({"type":"chunk","id":id,"index":if fault == 0 {0} else {2},"data":[2]})
                )
                .is_err()
            );
        }
    }
}
