import { ack } from "../web/protocol/wire.mjs";
import { ACK_INTERVAL } from "./limits.mjs";
// Endpoint-side receive accounting. Small keepalives use the byte threshold;
// callers explicitly flush at a transfer-burst boundary or before session close.
export class AckTracker {
  constructor(session) {
    this.session = session;
    this.received = 0n;
    this.acked = 0n;
  }
  receive(length) {
    if (!Number.isInteger(length) || length < 6 || length > 65541)
      throw Error("length");
    this.received += BigInt(length);
    return this.received - this.acked >= BigInt(ACK_INTERVAL)
      ? this.flush()
      : null;
  }
  flush() {
    if (this.received === this.acked) return null;
    const frame = ack(this.session, this.received);
    this.acked = this.received;
    return frame;
  }
}
