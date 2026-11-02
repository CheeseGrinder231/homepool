import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { createInterface } from "node:readline";
import { readFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { Session, keyPair, publicBytes, vectorKey } from "../noise.mjs";
import {
  encode,
  decode,
  envelope,
  unwrap,
  admission,
  Transfer,
  tokenSet,
  bindingBytes,
} from "../wire.mjs";
import schema from "../schema.mjs";
const hex = (b) => Buffer.from(b).toString("hex"),
  unhex = (s) => new Uint8Array(Buffer.from(s, "hex"));
const vectors = JSON.parse(
  await readFile(new URL("../../../protocol/cacophony.json", import.meta.url)),
);
const v = vectors.find((x) => x.protocol_name.includes("Noise_IK_"));
const target = process.env.CARGO_TARGET_DIR || `${process.env.TMPDIR}/target`;
const build = spawnSync(
  "cargo",
  [
    "build",
    "-p",
    "homepool-protocol",
    "--example",
    "endpoint",
    "--locked",
    "--offline",
  ],
  { env: { ...process.env, CARGO_TARGET_DIR: target }, encoding: "utf8" },
);
assert.equal(build.status, 0, build.stderr);
function endpoint(t) {
  const child = spawn(`${target}/debug/examples/endpoint`, [], {
    stdio: ["pipe", "pipe", "inherit"],
  });
  t.after(() => child.kill());
  const pending = [];
  createInterface({ input: child.stdout }).on("line", (s) => {
    const p = pending.shift();
    const v = JSON.parse(s);
    if (v.error) p.reject(Error(v.error));
    else p.resolve(v.ok);
  });
  child.on("exit", () => {
    for (const p of pending) p.reject(Error("endpoint exited"));
  });
  return (request) =>
    new Promise((resolve, reject) => {
      pending.push({ resolve, reject });
      child.stdin.write(JSON.stringify(request) + "\n");
    });
}
async function make(rpc, mode, initiator, session = 1) {
  const k = await keyPair();
  const pub = await publicBytes(k.publicKey);
  const s = await Session.create({
    mode,
    initiator,
    route: "test",
    privateKey: k.privateKey,
    publicKey: pub,
    remoteStatic: initiator ? unhex(v.init_remote_static) : undefined,
    psk: new Uint8Array(32),
  });
  await rpc({
    op: "new",
    mode,
    initiator: !initiator,
    session,
    route: "test",
    private: initiator ? v.resp_static : v.init_static,
    remote: initiator ? undefined : hex(pub),
    psk: "00".repeat(32),
  });
  const call = (op, data) =>
    rpc({ op, session, data: hex(data ?? new Uint8Array()) }).then(unhex);
  if (initiator) {
    await call("read", await s.writeHandshake());
    await s.readHandshake(await call("write"));
    if (mode === "enroll") await call("read", await s.writeHandshake());
  } else {
    await s.readHandshake(await call("write"));
    await call("read", await s.writeHandshake());
    if (mode === "enroll") await s.readHandshake(await call("write"));
  }
  return { s, call };
}
for (const mode of ["enroll", "reconnect"])
  for (const initiator of [true, false])
    test(`real processes ${mode} JS ${initiator ? "initiator" : "responder"}`, async (t) => {
      const { s, call } = await make(endpoint(t), mode, initiator);
      for (const n of [0, 1, 65519]) {
        const b = new Uint8Array(n).fill(123);
        assert.deepEqual(await call("decrypt", await s.encrypt(b)), b);
        assert.deepEqual(await s.decrypt(await call("encrypt", b)), b);
      }
      await assert.rejects(call("encrypt", new Uint8Array(65520)));
    });
test("IK replay cannot authorize transport", async (t) => {
  const rpc = endpoint(t),
    k = await keyPair(),
    pub = await publicBytes(k.publicKey);
  const js = await Session.create({
    mode: "reconnect",
    initiator: true,
    route: "test",
    privateKey: k.privateKey,
    publicKey: pub,
    remoteStatic: unhex(v.init_remote_static),
  });
  const m1 = await js.writeHandshake();
  const fresh = async (id) => {
    await rpc({
      op: "new",
      mode: "reconnect",
      session: id,
      initiator: false,
      route: "test",
      private: v.resp_static,
    });
    await rpc({ op: "read", session: id, data: hex(m1) });
    return unhex(await rpc({ op: "write", session: id }));
  };
  await js.readHandshake(await fresh(1));
  const operation = await js.encrypt(
    encode({ type: "list", id: "11".repeat(16), cursor: "", limit: 10 }),
  );
  await fresh(2);
  await assert.rejects(
    rpc({ op: "decrypt", session: 2, data: hex(operation) }),
  );
});
test("shared schema and all field vectors, refusals, envelopes and admission", async (t) => {
  assert.deepEqual(
    schema,
    JSON.parse(
      await readFile(new URL("../../../protocol/schema.json", import.meta.url)),
    ),
  );
  const rpc = endpoint(t);
  const samples = {
    id: "11".repeat(16),
    cursor: "",
    page: 10,
    key: "2026-01-01/010203-" + "a".repeat(26) + "-photo.jpg",
    length: 42,
    mime: "image/jpeg",
    name: "photo.jpg",
    data: new Uint8Array([1, 2, 3]),
    count: 1,
    items: [],
    hash: "ab".repeat(32),
    token: "cd".repeat(32),
    code: "refused",
    credit: 4,
  };
  for (const [type, fields] of schema) {
    const m = { type };
    for (const [f, k] of fields) m[f] = samples[k];
    const b = encode(m),
      json = JSON.parse(
        JSON.stringify(m, (_, v) =>
          v instanceof Uint8Array ? Array.from(v) : v,
        ),
      );
    assert.equal(await rpc({ op: "encode", message: json }), hex(b));
    assert.deepEqual(await rpc({ op: "decode", data: hex(b) }), json);
    assert.deepEqual(decode(b), m);
  }
  for (const m of [
    { type: "delete" },
    {
      type: "put-begin",
      id: samples.id,
      name: "x".repeat(256),
      type2: "x",
      length: 1,
    },
    { type: "list", id: samples.id, cursor: "x".repeat(1025), limit: 1 },
    { type: "list", id: samples.id, cursor: "", limit: 101 },
  ]) {
    assert.throws(() => encode(m));
    await assert.rejects(rpc({ op: "encode", message: m }));
  }
  for (const b of [
    new Uint8Array([1, 255]),
    new Uint8Array([2, 1]),
    new Uint8Array(65520),
  ]) {
    assert.throws(() => decode(b));
    await assert.rejects(rpc({ op: "decode", data: hex(b) }));
  }
  for (const kind of ["open", "data", "close", "error"]) {
    const b = envelope(kind, 1, kind === "open" ? new Uint8Array(4) : new Uint8Array());
    assert.equal(await rpc({ op: "envelope", data: hex(b) }), hex(b));
  }
  for (const role of ["phone", "bridge"]) {
    const b = admission(role, samples.token);
    assert.equal(await rpc({ op: "admission", data: hex(b) }), hex(b));
  }
  assert.throws(() => envelope("open", 1, new Uint8Array([1])));
  assert.throws(() => unwrap(new Uint8Array([1, 4, 0, 0, 0, 1])));
});
test("two phones, one Rust bridge, interleaved 10 MiB put/get through memory relay", async (t) => {
  const rpc = endpoint(t),
    a = await make(rpc, "enroll", true, 1),
    b = await make(rpc, "reconnect", true, 2);
  const id = "01".repeat(16),
    length = 10 * 1024 * 1024 + 7;
  const phone = new Transfer(id, length),
    bridge = new Transfer(id, length);
  const uploadHash = createHash("sha256"),
    storedHash = createHash("sha256"),
    downloadHash = createHash("sha256");
  let count = 0;
  const up = async (m) => {
    const framed = envelope("data", 1, await a.s.encrypt(encode(m)));
    const e = unwrap(unhex(await rpc({ op: "envelope", data: hex(framed) })));
    assert.equal(e.session, 1);
    return decode(await a.call("receive", e.data));
  };
  const down = async (m) => {
    const e = unwrap(envelope("data", 2, await b.call("encrypt", encode(m))));
    assert.equal(e.session, 2);
    return decode(await b.s.decrypt(e.data));
  };
  assert.equal(
    (
      await up({
        type: "put-begin",
        id,
        name: "photo.jpg",
        mime: "image/jpeg",
        length,
      })
    ).length,
    length,
  );
  const key = "2026-01-01/010203-" + "a".repeat(26) + "-photo.jpg";
  const request = decode(
    await b.call(
      "decrypt",
      await b.s.encrypt(encode({ type: "get", id, key })),
    ),
  );
  assert.equal(request.key, key);
  assert.equal(
    (await down({ type: "get-head", id, length, mime: "image/jpeg" })).length,
    length,
  );
  for (let offset = 0; offset < length; offset += 61440) {
    const data = new Uint8Array(Math.min(61440, length - offset));
    for (let j = 0; j < data.length; j++) data[j] = (offset + j) % 251;
    uploadHash.update(data);
    const received = await up({ type: "chunk", id, index: count, data });
    bridge.accept(received);
    storedHash.update(received.data);
    const got = await down({
      type: "chunk",
      id,
      index: count,
      data: received.data,
    });
    phone.accept(got);
    downloadHash.update(got.data);
    count++;
  }
  assert.equal(
    bridge.accept(await up({ type: "put-end", id, length, count })),
    true,
  );
  assert.equal(
    phone.accept(await down({ type: "get-end", id, length, count })),
    true,
  );
  const uploaded = uploadHash.digest("hex");
  assert.equal(uploaded, storedHash.digest("hex"));
  assert.equal(downloadHash.digest("hex"), uploaded);
});

test("shared relay control and Unicode vectors", async (t) => {
  const rpc = endpoint(t);
  const cases = JSON.parse(await readFile(new URL("../../../protocol/relay-vectors.json", import.meta.url)));
  for (const v of cases.envelopes) {
    const b = unhex(v.hex);
    assert.equal(hex(envelope(v.kind, v.session, unhex(v.data))), v.hex);
    assert.deepEqual(unwrap(b), {kind:v.kind, session:v.session, data:unhex(v.data)});
    assert.equal(await rpc({op:"envelope", data:v.hex}), v.hex);
  }
  for (const v of cases.envelopes.filter(v => v.kind === "token-set")) {
    const d = unhex(v.data), view = new DataView(d.buffer);
    assert.equal(hex(tokenSet(d.slice(0,32), d[32], view.getUint32(33), view.getBigUint64(37))), v.hex);
  }
  assert.equal(hex(bindingBytes(0xffffffff)), "ffffffff");
  assert.throws(() => bindingBytes(-1));
  assert.throws(() => tokenSet(new Uint8Array(32), 2, 0));
  for (const b of cases.decodeRefuse) {
    assert.throws(() => decode(unhex(b)));
    await assert.rejects(rpc({op:"decode", data:b}));
  }
  for (const m of cases.unicode) {
    const b = encode(m);
    assert.deepEqual(decode(b), m);
    assert.deepEqual(await rpc({op:"decode", data:hex(b)}), m);
    assert.equal(await rpc({op:"encode", message:m}), hex(b));
  }
  for (const m of cases.refuse) {
    assert.throws(() => encode(m));
    await assert.rejects(rpc({op:"encode", message:m}));
  }
  for (const hex of cases.invalid) {
    assert.throws(() => unwrap(unhex(hex)));
    await assert.rejects(rpc({op:"envelope", data:hex}));
  }
});
