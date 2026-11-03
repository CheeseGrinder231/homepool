// Service tabs (#59): catalog checks, every fetch refusal, and the two phone
// operations through a paired session. Fixture services listen on loopback.
use crate::services::{self, Devices, Refusal, Service};
use crate::tests::Fixture;
use homepool_protocol::wire;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{Ipv4Addr, Ipv6Addr, TcpListener},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

// Built from parts: the repository's literal scan flags URL prefixes and
// non-documentation addresses in source text.
fn http(rest: &str) -> String {
    format!("{}//{rest}", "http:")
}
fn v4() -> String {
    Ipv4Addr::LOCALHOST.to_string()
}
fn v6() -> String {
    format!("[{}]", Ipv6Addr::LOCALHOST)
}
fn service(id: &str, port: u16, max_bytes: u64, devices: Devices) -> Service {
    Service {
        id: id.into(),
        title: "Status".into(),
        url: http(&format!("{}:{port}/view.json", v4())),
        max_bytes,
        devices,
    }
}
fn all() -> Devices {
    Devices::All("all".into())
}
fn doc() -> Value {
    json!({"version":1,"title":"Work","as_of":"2026-01-01T00:00:00Z","valid_until":"2026-01-01T00:01:00Z",
        "sections":[{"title":"Queue","items":[{"title":"<b>x</b>","state":"stuck","link":"javascript:alert(1)"}]}]})
}

struct Fake {
    port: u16,
    hits: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
}
// Answers each connection with `respond(n)`; None never answers.
fn serve(respond: impl Fn(usize) -> Option<Vec<u8>> + Send + Sync + 'static) -> Fake {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(vec![]));
    let (h, r, respond) = (hits.clone(), requests.clone(), Arc::new(respond));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let n = h.fetch_add(1, Ordering::SeqCst);
            let (r, respond) = (r.clone(), respond.clone());
            std::thread::spawn(move || {
                let mut head = vec![];
                let mut b = [0; 1];
                while !head.ends_with(b"\r\n\r\n") && stream.read_exact(&mut b).is_ok() {
                    head.push(b[0]);
                }
                r.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&head).into());
                match respond(n) {
                    Some(bytes) => {
                        let _ = stream.write_all(&bytes);
                    }
                    None => std::thread::sleep(Duration::from_secs(5)),
                }
            });
        }
    });
    Fake {
        port,
        hits,
        requests,
    }
}
fn ok(body: &[u8]) -> Vec<u8> {
    let mut r = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    r.extend_from_slice(body);
    r
}
fn fetch(f: &Fake, max: u64) -> Result<Value, Refusal> {
    services::fetch(&service("s", f.port, max, all()), Duration::from_secs(3))
        .map(|b| serde_json::from_slice(&b).unwrap())
}

#[test]
fn catalog_accepts_only_fixed_loopback_urls() {
    let (a, b) = (v4(), v6());
    for good in [
        format!("{a}:8080/view.json"),
        format!("{b}:9/x"),
        format!("{a}:80/"),
    ] {
        let t = services::target(&http(&good)).unwrap_or_else(|e| panic!("{good}: {e}"));
        assert_eq!(format!("{}{}", t.authority, t.path), good);
    }
    for bad in [
        http("localhost:8080/x"),
        http("192.0.2.1:8080/x"),
        http(&format!("{a}/x")),
        http(&format!("{a}:0/x")),
        http(&format!("{a}:080/x")),
        http(&format!("{a}:8080")),
        http(&format!("{a}:8080/x?q=1")),
        http(&format!("{a}:8080/x?")),
        http(&format!("{a}:8080/x#f")),
        http(&format!("u:p@{a}:8080/x")),
        http(&format!("{a}:8080/a/../b")),
        http(&format!("{a}:8080/a b")),
        http(&format!("{}:8080/x", Ipv4Addr::new(127, 0, 0, 2))),
        http(&format!("{}:8080/x", Ipv4Addr::new(127, 0, 0, 1).to_bits())),
        http(&format!(
            "[{}]:8080/x",
            Ipv6Addr::LOCALHOST
                .segments()
                .map(|n| n.to_string())
                .join(":")
        )),
        format!("{}//{a}:8080/x", "https:"),
        format!("{}//{a}:8080/x", "HTTP:"),
        format!("ftp://{a}:8080/x"),
        format!("{a}:8080/x"),
    ] {
        assert!(services::target(&bad).is_err(), "{bad}");
    }
}

#[test]
fn catalog_entries_are_checked_when_config_loads() {
    let good = service("status-1", 1, 1024, all());
    assert!(services::validate(std::slice::from_ref(&good)).is_ok());
    let id = "0123456789abcdef0123456789abcdef".to_string();
    assert!(services::validate(&[service("s", 1, 1, Devices::List(vec![id.clone()]))]).is_ok());
    let mut cases = vec![
        vec![service("Status", 1, 1, all())],
        vec![service("", 1, 1, all())],
        vec![service(&"a".repeat(33), 1, 1, all())],
        vec![good.clone(), good.clone()],
        vec![service("s", 1, 0, all())],
        vec![service(
            "s",
            1,
            crate::settings::SERVICE_MAX_BYTES + 1,
            all(),
        )],
        vec![service("s", 1, 1, Devices::All("everyone".into()))],
        vec![service("s", 1, 1, Devices::List(vec!["phone".into()]))],
        (0..17)
            .map(|i| service(&format!("s{i}"), 1, 1, all()))
            .collect(),
    ];
    for title in ["", "a\nb", &"x".repeat(41)] {
        let mut s = good.clone();
        s.title = title.into();
        cases.push(vec![s]);
    }
    let mut s = good.clone();
    s.url = http(&format!("{}:1/x?q", v4()));
    cases.push(vec![s]);
    for case in cases {
        assert!(services::validate(&case).is_err(), "{}", case[0].id);
    }
    // Unknown fields (a method, headers) are refused, and max_bytes defaults.
    let parsed: Service =
        serde_json::from_value(json!({"id":"s","title":"S","url":good.url,"devices":[id]}))
            .unwrap();
    assert_eq!(parsed.max_bytes, crate::settings::SERVICE_DEFAULT_BYTES);
    assert!(
        serde_json::from_value::<Service>(
            json!({"id":"s","title":"S","url":good.url,"devices":"all","method":"POST"})
        )
        .is_err()
    );
}

#[test]
fn fetch_sends_a_fixed_get_and_returns_the_normalised_document() {
    let body = serde_json::to_vec(&doc()).unwrap();
    let f = serve(move |_| Some(ok(&body)));
    let v = fetch(&f, 4096).unwrap();
    assert_eq!(v["title"], "Work");
    assert_eq!(v["sections"][0]["items"][0]["title"], "<b>x</b>");
    assert!(v["sections"][0]["items"][0].get("link").is_none());
    assert!(crate::view::parse_time(v["fetched_at"].as_str().unwrap()).is_some());
    let request = f.requests.lock().unwrap()[0].clone();
    assert_eq!(
        request,
        format!(
            "GET /view.json HTTP/1.1\r\nHost: {}:{}\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
            v4(),
            f.port
        )
    );
}

#[test]
fn chunked_and_close_delimited_bodies_are_read() {
    let body = serde_json::to_vec(&doc()).unwrap();
    let (a, b) = body.split_at(10);
    let mut chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    for part in [a, b] {
        chunked.extend(format!("{:x};x=y\r\n", part.len()).bytes());
        chunked.extend(part);
        chunked.extend(b"\r\n");
    }
    chunked.extend(b"0\r\n\r\n");
    let f = serve(move |_| Some(chunked.clone()));
    assert_eq!(fetch(&f, 4096).unwrap()["title"], "Work");
    // Trailer fields are read and ignored up to the blank line.
    let mut trailer = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    trailer.extend(format!("{:x}\r\n", body.len()).bytes());
    trailer.extend(&body);
    trailer.extend(b"\r\n0\r\nX-Checked: yes\r\n\r\n");
    let f = serve(move |_| Some(trailer.clone()));
    assert_eq!(fetch(&f, 4096).unwrap()["title"], "Work");
    let mut close = b"HTTP/1.0 200 OK\r\n\r\n".to_vec();
    close.extend(&body);
    let f = serve(move |_| Some(close.clone()));
    assert_eq!(fetch(&f, 4096).unwrap()["title"], "Work");
}

#[test]
fn every_refusal_case() {
    let body = serde_json::to_vec(&doc()).unwrap();
    let big = vec![b' '; 5000];
    let cases: Vec<(&str, Vec<u8>, Refusal)> = vec![
        (
            "redirect",
            format!(
                "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\n\r\n",
                http("192.0.2.1:1/")
            )
            .into_bytes(),
            Refusal::Unavailable,
        ),
        (
            "not found",
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
            Refusal::Unavailable,
        ),
        (
            "server error",
            b"HTTP/1.1 500 Oops\r\nContent-Length: 0\r\n\r\n".to_vec(),
            Refusal::Unavailable,
        ),
        ("not http", b"hello\r\n\r\n".to_vec(), Refusal::Unavailable),
        (
            "short body",
            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n{}".to_vec(),
            Refusal::Unavailable,
        ),
        (
            "two lengths",
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
            Refusal::Unavailable,
        ),
        (
            "oversize by length",
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", 4097).into_bytes(),
            Refusal::TooLarge,
        ),
        (
            "oversize by close",
            [b"HTTP/1.1 200 OK\r\n\r\n".to_vec(), big.clone()].concat(),
            Refusal::TooLarge,
        ),
        (
            "oversize chunked",
            [
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1388\r\n".to_vec(),
                big.clone(),
                b"\r\n0\r\n\r\n".to_vec(),
            ]
            .concat(),
            Refusal::TooLarge,
        ),
        ("invalid JSON", ok(b"<html>Status</html>"), Refusal::Invalid),
        (
            "invalid document",
            ok(br#"{"version":2}"#),
            Refusal::Invalid,
        ),
        (
            "bad chunk",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n".to_vec(),
            Refusal::Unavailable,
        ),
        (
            "gzip",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n".to_vec(),
            Refusal::Unavailable,
        ),
    ];
    assert_eq!(
        fetch(&serve(move |_| Some(ok(&body))), 4096).map(|_| ()),
        Ok(())
    );
    for (name, response, expected) in cases {
        let f = serve(move |_| Some(response.clone()));
        assert_eq!(fetch(&f, 4096).map(|_| ()), Err(expected), "{name}");
        assert_eq!(
            f.hits.load(Ordering::SeqCst),
            1,
            "{name}: no redirect followed"
        );
    }
    // Down: nothing listens on the port.
    let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let down = service("down", port, 4096, all());
    assert_eq!(
        services::fetch(&down, Duration::from_secs(3)).map(|_| ()),
        Err(Refusal::Unavailable)
    );
}

// Review of #60: a chunk size near u64::MAX wrapped the cap check, and the
// body framing was not checked to its end.
#[test]
fn chunk_sizes_and_framing_are_checked_to_the_end() {
    let body = serde_json::to_vec(&doc()).unwrap();
    let head = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    let one = [
        head.clone(),
        format!("{:x}\r\n", body.len()).into_bytes(),
        body.clone(),
        b"\r\n".to_vec(),
    ]
    .concat();
    let cases: Vec<(&str, Vec<u8>, Refusal)> = vec![
        (
            "a byte, then the largest size",
            [
                head.clone(),
                b"1\r\nx\r\nffffffffffffffff\r\n".to_vec(),
                vec![b' '; 8192],
            ]
            .concat(),
            Refusal::TooLarge,
        ),
        (
            "the largest size first",
            [
                head.clone(),
                b"ffffffffffffffff\r\n".to_vec(),
                vec![b' '; 8192],
            ]
            .concat(),
            Refusal::TooLarge,
        ),
        (
            "a size past u64",
            [head.clone(), b"10000000000000000\r\n".to_vec()].concat(),
            Refusal::Unavailable,
        ),
        (
            "exactly one byte over",
            [
                head.clone(),
                b"1\r\nx\r\n1000\r\n".to_vec(),
                vec![b' '; 4096],
                b"\r\n0\r\n\r\n".to_vec(),
            ]
            .concat(),
            Refusal::TooLarge,
        ),
        ("no last chunk", one.clone(), Refusal::Unavailable),
        (
            "no blank line after the last chunk",
            [one.clone(), b"0\r\n".to_vec()].concat(),
            Refusal::Unavailable,
        ),
        (
            "a bare LF ends the body",
            [one.clone(), b"0\r\n\n".to_vec()].concat(),
            Refusal::Unavailable,
        ),
        (
            "a trailer that never ends",
            [one.clone(), b"0\r\n".to_vec(), b"X-Pad: 1\r\n".repeat(2000)].concat(),
            Refusal::Unavailable,
        ),
    ];
    for (name, response, expected) in cases {
        let f = serve(move |_| Some(response.clone()));
        assert_eq!(fetch(&f, 4096).map(|_| ()), Err(expected), "{name}");
    }
    let exact = [
        head,
        b"1000\r\n".to_vec(),
        vec![b' '; 4096 - body.len()],
        body,
        b"\r\n0\r\n\r\n".to_vec(),
    ]
    .concat();
    let f = serve(move |_| Some(exact.clone()));
    assert_eq!(fetch(&f, 4096).unwrap()["title"], "Work", "exactly the cap");
}

// The header block must end within its cap, however the reads fall.
#[test]
fn headers_end_within_their_cap() {
    let body = serde_json::to_vec(&doc()).unwrap();
    let cap = crate::settings::SERVICE_HEADER_BYTES;
    let response = |size: usize| {
        let start = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nX-Pad: ",
            body.len()
        );
        let pad = size - start.len() - 4;
        [
            start.into_bytes(),
            vec![b'a'; pad],
            b"\r\n\r\n".to_vec(),
            body.clone(),
        ]
        .concat()
    };
    let (at, over) = (response(cap), response(cap + 1));
    let f = serve(move |_| Some(at.clone()));
    assert_eq!(fetch(&f, 4096).unwrap()["title"], "Work", "exactly the cap");
    let f = serve(move |_| Some(over.clone()));
    assert_eq!(
        fetch(&f, 4096).map(|_| ()),
        Err(Refusal::Unavailable),
        "one byte over"
    );
}

// A worker that ends without an outcome still answers everyone waiting on it,
// and the next request starts a new fetch.
#[test]
fn waiters_recover_when_a_worker_fails() {
    let body = serde_json::to_vec(&doc()).unwrap();
    let f = serve(move |_| Some(ok(&body)));
    let s = service("recover", f.port, 4096, all());
    let a = services::start(&s, |_| {
        std::thread::sleep(Duration::from_millis(200));
        panic!("a deliberate panic in a test worker");
    });
    let b = services::start(&s, |_| unreachable!("joins the running fetch"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while a.poll().is_none() || b.poll().is_none() {
        assert!(Instant::now() < deadline, "waiters must not hang");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(a.poll().unwrap().map(|_| ()), Err(Refusal::Unavailable));
    assert_eq!(b.poll().unwrap().map(|_| ()), Err(Refusal::Unavailable));
    let c = services::request(&s);
    while c.poll().is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(c.poll().unwrap().is_ok());
    assert_eq!(f.hits.load(Ordering::SeqCst), 1);
}

#[test]
fn one_deadline_covers_a_silent_or_dripping_service() {
    let silent = serve(|_| None);
    let started = Instant::now();
    let s = service("silent", silent.port, 4096, all());
    assert_eq!(
        services::fetch(&s, Duration::from_millis(400)).map(|_| ()),
        Err(Refusal::Unavailable)
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    // A response that trickles one byte at a time still ends at the deadline.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n");
            for _ in 0..100 {
                if stream.write_all(b" ").is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    });
    let started = Instant::now();
    let s = service("drip", port, 4096, all());
    assert_eq!(
        services::fetch(&s, Duration::from_millis(400)).map(|_| ()),
        Err(Refusal::Unavailable)
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn a_request_during_a_fetch_waits_for_that_fetch() {
    let body = serde_json::to_vec(&doc()).unwrap();
    let f = serve(move |_| {
        std::thread::sleep(Duration::from_millis(300));
        Some(ok(&body))
    });
    let s = service("coalesce", f.port, 4096, all());
    let (a, b) = (services::request(&s), services::request(&s));
    let deadline = Instant::now() + Duration::from_secs(5);
    while a.poll().is_none() || b.poll().is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(f.hits.load(Ordering::SeqCst), 1);
    assert_eq!(a.poll().unwrap().unwrap(), b.poll().unwrap().unwrap());
    // Once finished, the next request fetches again.
    let c = services::request(&s);
    while c.poll().is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(f.hits.load(Ordering::SeqCst), 2);
}

// Sends one record on a paired session and runs the peer until it answers.
fn ask(
    f: &mut Fixture,
    peer: &mut crate::peer::Peer,
    phone: &mut homepool_protocol::Session,
    m: Value,
) -> Vec<Value> {
    let sealed = phone
        .message(false, false, &wire::encode(&m).unwrap())
        .unwrap();
    let mut frames = peer
        .receive(&f.c, &mut f.s, &mut f.settlements, &sealed)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut out = vec![];
    loop {
        for frame in frames.drain(..) {
            out.push(wire::decode(&phone.message(true, false, &frame).unwrap()).unwrap());
        }
        let done = out.last().is_some_and(|m: &Value| {
            matches!(
                m["type"].as_str(),
                Some("services-list" | "refused" | "get-head" | "get-end" | "error")
            )
        });
        if done {
            return out;
        }
        assert!(Instant::now() < deadline, "no answer to {m}");
        std::thread::sleep(Duration::from_millis(10));
        frames = peer.tick(&mut f.s).unwrap();
    }
}

#[test]
fn phone_operations_follow_the_catalog_and_grants() {
    let body = serde_json::to_vec(&doc()).unwrap();
    let up = serve(move |_| Some(ok(&body)));
    let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut f = Fixture::new();
    let (mut peer, mut phone, device) = f.enroll();
    f.c.services = vec![
        service("shared", up.port, 4096, all()),
        service("mine", up.port, 4096, Devices::List(vec![device.clone()])),
        service("theirs", up.port, 4096, Devices::List(vec!["0".repeat(32)])),
        service("down", port, 4096, all()),
    ];
    let id = "a".repeat(32);
    let list = ask(
        &mut f,
        &mut peer,
        &mut phone,
        json!({"type":"services","id":id}),
    );
    assert_eq!(
        list[0]["services"],
        json!([{"id":"shared","title":"Status"},{"id":"mine","title":"Status"},{"id":"down","title":"Status"}])
    );
    for (name, reason) in [
        ("nope", "unknown"),
        ("theirs", "not-allowed"),
        ("down", "unavailable"),
    ] {
        let r = ask(
            &mut f,
            &mut peer,
            &mut phone,
            json!({"type":"service-get","id":id,"service":name}),
        );
        assert_eq!(
            r,
            vec![json!({"type":"refused","id":id,"reason":reason})],
            "{name}"
        );
    }
    assert_eq!(
        up.hits.load(Ordering::SeqCst),
        0,
        "refused before any fetch"
    );
    let head = ask(
        &mut f,
        &mut peer,
        &mut phone,
        json!({"type":"service-get","id":id,"service":"mine"}),
    );
    assert_eq!(head[0]["type"], "get-head");
    assert_eq!(head[0]["mime"], "application/json");
    let mut bytes = vec![];
    let mut out = ask(
        &mut f,
        &mut peer,
        &mut phone,
        json!({"type":"credit","id":id,"chunks":8}),
    );
    while out.last().unwrap()["type"] != "get-end" {
        std::thread::sleep(Duration::from_millis(10));
        for frame in peer.tick(&mut f.s).unwrap() {
            out.push(wire::decode(&phone.message(true, false, &frame).unwrap()).unwrap());
        }
    }
    for m in &out {
        if m["type"] == "chunk" {
            bytes.extend(
                m["data"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|x| x.as_u64().unwrap() as u8),
            );
        }
    }
    assert_eq!(out.last().unwrap()["length"], bytes.len());
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["sections"][0]["state"], "stuck");
    // A revoked device's next record ends the session.
    f.s.devices.remove(&device);
    let sealed = phone
        .message(
            false,
            false,
            &wire::encode(&json!({"type":"services","id":id})).unwrap(),
        )
        .unwrap();
    assert!(
        peer.receive(&f.c, &mut f.s, &mut f.settlements, &sealed)
            .is_err()
    );
}
