import schema from "./schema.mjs";
import { concat } from "./noise.mjs";
const enc = new TextEncoder(),
  dec = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });
const bounds = {
  length: 2 ** 53 - 1,
  count: 0xffffffff,
  page: 100,
  credit: 16,
};
const sizes = {
  id: 32,
  token: 64,
  hash: 64,
  key: 512,
  name: 255,
  mime: 127,
  cursor: 1024,
  code: 32,
};
export function valid(kind, value) {
  if (kind in bounds) {
    if (
      !Number.isSafeInteger(value) ||
      value < 0 ||
      value > bounds[kind] ||
      (["page", "credit"].includes(kind) && !value)
    )
      throw Error("integer");
    return;
  }
  if (kind === "data") {
    if (!(value instanceof Uint8Array) || !value.length || value.length > 61440)
      throw Error("chunk");
    return;
  }
  if (kind === "items") {
    if (!Array.isArray(value) || value.length > 100) throw Error("items");
    for (const item of value) {
      if (Object.keys(item).sort().join(",") !== "key,length,type")
        throw Error("item fields");
      for (const [f, k] of [
        ["key", "key"],
        ["length", "length"],
        ["type", "mime"],
      ])
        valid(k, item[f]);
    }
    return;
  }
  if (
    typeof value !== "string" ||
    enc.encode(value).length > sizes[kind] ||
    /[\x00-\x1f\x7f]/.test(value)
  )
    throw Error("string");
  if (
    ["id", "hash", "token"].includes(kind) &&
    !new RegExp(`^[0-9a-f]{${sizes[kind]}}$`).test(value)
  )
    throw Error("hex");
  if (kind === "name" && !value.length) throw Error("name");
  if (kind === "mime" && !/^[\x20-\x7e]*$/.test(value)) throw Error("mime");
  if (kind === "code" && !/^[a-z-]{1,32}$/.test(value))
    throw Error("error code");
  if (
    kind === "key" &&
    !/^\d{4}-\d{2}-\d{2}\/\d{6}-[a-z2-7]{25}[aeimquy4]-[A-Za-z0-9_](?:[A-Za-z0-9._-]{0,71}[A-Za-z0-9_])?$/.test(
      value,
    )
  )
    throw Error("key");
}
function number(n, width) {
  const b = new Uint8Array(width),
    v = new DataView(b.buffer);
  if (width === 8) v.setBigUint64(0, BigInt(n));
  else if (width === 4) v.setUint32(0, n);
  else v.setUint16(0, n);
  return b;
}
function field(kind, value) {
  valid(kind, value);
  if (kind in bounds) return number(value, kind === "length" ? 8 : 4);
  if (kind === "items")
    return concat(
      number(value.length, 2),
      ...value.flatMap((i) => [
        field("key", i.key),
        field("length", i.length),
        field("mime", i.type),
      ]),
    );
  const b = kind === "data" ? value : enc.encode(value);
  return concat(number(b.length, 2), b);
}
export function encode(message) {
  const index = schema.findIndex(([name]) => name === message.type);
  if (index < 0) throw Error("unknown type");
  const fields = schema[index][1];
  if (
    Object.keys(message).sort().join(",") !==
    ["type", ...fields.map(([f]) => f)].sort().join(",")
  )
    throw Error("fields");
  const b = concat(
    new Uint8Array([1, index + 1]),
    ...fields.map(([f, k]) => field(k, message[f])),
  );
  if (b.length > 65519) throw Error("size");
  return b;
}
export function decode(b) {
  if (
    !(b instanceof Uint8Array) ||
    b.length > 65519 ||
    b[0] !== 1 ||
    !schema[b[1] - 1]
  )
    throw Error("header");
  let at = 2;
  const take = (n) => {
    if (at + n > b.length) throw Error("truncated");
    const x = b.slice(at, at + n);
    at += n;
    return x;
  };
  const num = (n) => {
    const x = take(n),
      v = new DataView(x.buffer);
    return n === 8
      ? Number(v.getBigUint64(0))
      : n === 4
        ? v.getUint32(0)
        : v.getUint16(0);
  };
  const read = (k) => {
    let v;
    if (k in bounds) v = num(k === "length" ? 8 : 4);
    else if (k === "items") {
      const n = num(2);
      if (n > 100) throw Error("items");
      v = Array.from({ length: n }, () => ({
        key: read("key"),
        length: read("length"),
        type: read("mime"),
      }));
    } else {
      const x = take(num(2));
      v = k === "data" ? x : dec.decode(x);
    }
    valid(k, v);
    return v;
  };
  const [type, fields] = schema[b[1] - 1],
    out = { type };
  for (const [f, k] of fields) out[f] = read(k);
  if (at !== b.length) throw Error("trailing");
  return out;
}
// Relay controls have fixed positional payloads; session zero is privileged.
const kinds = ["data", "open", "close", "error", "token-set", "token-remove", "ping", "pong"];
export function envelope(kind, session, data = new Uint8Array()) {
  const k = kinds.indexOf(kind);
  const sizes = [null, 4, 0, 0, 45, 4, 8, 8];
  if (k < 0 || !Number.isInteger(session) || session < 0 || session > 0xffffffff ||
      !(data instanceof Uint8Array) || data.length > 65535 ||
      (k >= 4 ? session !== 0 : session === 0) ||
      (k !== 0 && data.length !== sizes[k]) || (k === 4 && data[32] > 1))
    throw Error("envelope");
  return concat(new Uint8Array([1, k]), number(session, 4), data);
}
export function unwrap(b) {
  if (!(b instanceof Uint8Array) || b.length < 6 || b.length > 65541 || b[0] !== 1)
    throw Error("envelope");
  const kind = kinds[b[1]],
    session = new DataView(b.buffer, b.byteOffset, b.byteLength).getUint32(2),
    data = b.slice(6);
  envelope(kind, session, data);
  return { kind, session, data };
}
export function tokenSet(hash, tokenClass, binding, expiry = 0n) {
  if (!(hash instanceof Uint8Array) || hash.length !== 32 || ![0, 1].includes(tokenClass) ||
      !Number.isInteger(binding) || binding < 0 || binding > 0xffffffff ||
      typeof expiry !== "bigint" || expiry < 0n || expiry > 0xffffffffffffffffn)
    throw Error("token-set");
  return envelope("token-set", 0, concat(hash, new Uint8Array([tokenClass]), number(binding, 4), number(expiry, 8)));
}
export const bindingBytes = (binding) => {
  valid("count", binding);
  return number(binding, 4);
};
export function admission(role, token) {
  if (!["phone", "bridge"].includes(role)) throw Error("role");
  valid("token", token);
  return concat(
    new Uint8Array([1, role === "phone" ? 0 : 1]),
    Uint8Array.from(token.match(/../g), (x) => parseInt(x, 16)),
  );
}
export function readAdmission(b) {
  if (b.length !== 34 || b[0] !== 1 || b[1] > 1) throw Error("admission");
  return {
    role: b[1] ? "bridge" : "phone",
    token: Array.from(b.slice(2), (x) => x.toString(16).padStart(2, "0")).join(
      "",
    ),
  };
}
// One instance per transfer, only after get-head or an accepted put-begin.
export class Transfer {
  constructor(id, length) {
    valid("id", id);
    valid("length", length);
    Object.assign(this, { id, length, received: 0, count: 0, closed: false });
  }
  accept(m) {
    if (this.closed) throw Error("transfer closed");
    try {
      if (m.id !== this.id) throw Error("transfer ID");
      if (m.type === "chunk") {
        valid("data", m.data);
        if (
          m.index !== this.count ||
          this.received + m.data.length > this.length
        )
          throw Error("chunk order/size");
        this.count++;
        this.received += m.data.length;
        return false;
      }
      if (
        !["put-end", "get-end"].includes(m.type) ||
        m.length !== this.length ||
        m.length !== this.received ||
        m.count !== this.count
      )
        throw Error("end mismatch");
      this.closed = true;
      return true;
    } catch (e) {
      this.closed = true;
      throw e;
    }
  }
}
