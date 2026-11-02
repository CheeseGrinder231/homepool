import { Relay } from "../route.mjs";
import { defaults } from "../limits.mjs";
export class Socket {
  constructor() {
    this.readyState = 1;
    this.sent = [];
    this.data = null;
  }
  serializeAttachment(a) {
    this.data = structuredClone(a);
  }
  deserializeAttachment() {
    return structuredClone(this.data);
  }
  send(b) {
    if (this.readyState !== 1) throw Error("closed");
    this.sent.push(new Uint8Array(b));
  }
  close() {
    this.readyState = 3;
  }
}
export function harness(overrides = {}) {
  const values = new Map(),
    sockets = [];
  let tail = Promise.resolve();
  const ctx = {
    storage: {
      get: async (k) => structuredClone(values.get(k)),
      put: async (k, v) => {
        values.set(k, structuredClone(v));
      },
      setAlarm: async (n) => {
        ctx.alarm = n;
      },
      deleteAlarm: async () => {
        ctx.alarm = null;
      },
    },
    getWebSockets: () => sockets,
    acceptWebSocket: (s) => sockets.push(s),
    blockConcurrencyWhile: (f) => {
      const task = tail.then(f);
      tail = task.catch(() => {});
      return task;
    },
  };
  const env = {
    KILL_SWITCH: "false",
    BRIDGE_TOKENS: JSON.stringify({ example: "ab".repeat(32) }),
  };
  let relay = new Relay(ctx, env);
  relay.limits = { ...defaults, ...overrides };
  return {
    ctx,
    env,
    values,
    sockets,
    get relay() {
      return relay;
    },
    restart() {
      const l = relay.limits;
      relay = new Relay(ctx, env);
      relay.limits = l;
    },
    pending() {
      const s = new Socket();
      s.serializeAttachment({
        role: "pending",
        route: "example",
        deadline: Date.now() + 5000,
      });
      sockets.push(s);
      return s;
    },
    async send(s, b) {
      await relay.webSocketMessage(
        s,
        b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength),
      );
    },
  };
}
