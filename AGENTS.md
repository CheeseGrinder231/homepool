# Contributor and agent rules

- This repository is public. Never publish machine names, hostnames, private IP addresses, home-directory paths, usernames, email addresses, private repository names, internal issue details, or details of a contributor's host. Use generic terms such as a hub, a member, and machine A. Address examples must use RFC 5737 or RFC 3849 documentation ranges.
- Keep core pure: no I/O and no internal dependencies. Adapters depend on core only, never another adapter. Applications may depend on core and adapters.
- Assign every workspace crate a layer in `structure/layers.toml`. Do not add a crate without an assignment. Future adapters belong under `crates/adapters/`.
- Run `cargo xtask structure` before pushing. Keep all baselines at or below their previous values. Existing allowances are immutable; delete them when retired. See `docs/structure.md` for allowance requirements.
- Keep PRs small and focused. Run formatting, clippy, and workspace tests for Rust changes.
- Keep CI on GitHub-hosted `ubuntu-latest` runners with read-only contents permissions. Pin actions to full commit SHAs with version comments. Do not use secrets or privileged PR triggers.
- Do not set or override Git identity.
- Report vulnerabilities privately through GitHub's "Report a vulnerability" page, never in public issues. See `SECURITY.md` for the reporting link and required details.
