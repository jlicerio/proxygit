# Contributing to ProxyGit

Thanks for helping. This project is a small Rust workspace — keep changes
reviewable and environment-agnostic.

## Setup

```bash
cargo build --release
TMPDIR=/tmp cargo test -p proxygit-server -p proxygit-client -p proxygit-common
```

See [`QUICKSTART.md`](QUICKSTART.md) to run a local server.

## Before you open a PR

1. Run the verification gate in [`AGENTS.md`](AGENTS.md).
2. Add or extend tests for any new observable contract (protocol message, CLI
   verb, durability path).
3. Keep docs free of personal hostnames, tailnet IPs, and machine-local paths.
4. Do not present unauthenticated WebDAV/QUIC as internet-safe.
5. Match existing module style; avoid drive-by refactors.

## Design docs

Long-form research lives under `docs/design/`. Runtime behavior changes belong
in code + `SPEC.md` / `ARCHITECTURE-ROADMAP.md` as appropriate.

## License

By contributing, you agree that your contributions are licensed under the
Apache License 2.0 (`LICENSE`).
