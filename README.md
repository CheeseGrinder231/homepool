# homepool

Pool your own machines into one personal cloud.

homepool joins the computers you already own into a single pool. A lean hub keeps the registry of members and decides where work goes. Members add what they offer: compute for sandboxes, a portion of their disk for shared storage, and services. Everything else stays on the machine it belongs to.

**Status:** member-file status, Docker sandbox management through a local hub, and Garage storage commands work today; there is no release yet.
`homepool status` reads a private member file and reports its declared contributions;
see [the member file guide](docs/members.md) for setup and the versioned JSON format.

The [local hub](docs/hub.md) places sandboxes over an owner-only Unix socket and
rebuilds accounting from member inventory after a restart.
A separate [read-only HTTP API](docs/api.md) provides authenticated sandbox inventory
on loopback.

The [Garage storage guide](docs/garage.md) covers `stage-storage`, `apply-storage`
and `create-consumer`. They use an existing Garage installation directly, with
consumer credentials written only to a new private file. **A single-node layout
keeps a single copy. It is not a backup.**

## Principles

- The hub knows and decides. It holds no data and runs no workloads.
- Address capabilities, never machines.
- Share finished, immutable files. Live state stays with its owner.
- The hub opens no network listener. It reaches remote members over existing SSH access, so remote members must accept SSH from the hub.
- Structure checks from the first commit: layers point one way, no dependency cycles, no machine-specific data in code.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
