# homepool

Pool your own machines into one personal cloud.

homepool joins the computers you already own into a single pool. A lean hub keeps the registry of members and decides where work goes. Members add what they offer: compute for sandboxes, a portion of their disk for shared storage, and services. Everything else stays on the machine it belongs to.

**Status:** early. The workspace has structure checks and a pure pool model.
`homepool status` reads a private member file and reports its declared contributions;
see [the member file guide](docs/members.md) for setup and the versioned JSON format.

## Principles

- The hub knows and decides. It holds no data and runs no workloads.
- Address capabilities, never machines.
- Share finished, immutable files. Live state stays with its owner.
- Members dial out. No inbound ports or VPN required.
- Structure checks from the first commit: layers point one way, no dependency cycles, no machine-specific data in code.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
