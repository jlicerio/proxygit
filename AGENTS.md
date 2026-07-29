# ProxyGit — Agent Conventions

Instructions for coding agents (and humans) working **in this repository**.
Runtime agents that *consume* a live ProxyGit mount should read the workspace
`.proxygit` manifest instead — see [`QUICKSTART.md`](QUICKSTART.md).

## Mission

Keep ProxyGit correct, portable, and explainable. Prefer boring Rust, explicit
errors, and docs that a stranger can follow on a fresh machine with no VPN and
no personal hostnames.

## Verification gate

Every change must pass before claiming done:

```bash
cargo check --release -p proxygit-server -p proxygit-client
TMPDIR=/tmp cargo test -p proxygit-server -p proxygit-client -p proxygit-common
cargo fmt --check
```

Smoke the path you touched (CLI verb, WebDAV, MCP, or server boot) — green
units alone are not enough for transport/IO changes.

## Architecture decisions (current)

| Choice | Detail |
|--------|--------|
| Transport | **QUIC** via `quinn` — UDP 8080 |
| Human mount | **WebDAV** HTTP — TCP 3900 (no kext) |
| Agent API | **MCP** — stdio (`proxygit-client mcp`) and optional TCP 8082 |
| Index | **SQLite** per project on the server |
| Chunking | **FastCDC** |
| Hashing | **BLAKE3** |
| Blocks (MVP) | Local filesystem under `$PROXYGIT_DATA_DIR/blocks` |
| Compression | Zstd + VCDIFF planned; not required for MVP |

## Repo map

```
proxygit/
├── crates/
│   ├── proxygit-server/     # QUIC, WebDAV, SQLite, block store, embeddings MVP
│   ├── proxygit-client/     # CLI, WAL, MCP, optional FUSE
│   └── proxygit-common/     # shared types, CDC, frame protocol
├── docker/                  # portable server image + compose
├── scripts/                 # installers, local demo
├── docs/design/             # research / future design (not runtime-critical)
├── docs/reviews/            # historical review notes
├── SPEC.md                  # system specification
├── ARCHITECTURE-ROADMAP.md  # priority matrix
├── QUICKSTART.md            # user-facing runbook
└── README.md                # problem statement + share entrypoint
```

## Coding rules

1. **No environment lock-in in docs or defaults committed upstream.**  
   Forbidden in tree-facing docs: personal home paths, one-off hostnames
   (`linuxbox`), bare tailnet IPs, single-vendor orchestrator hooks as
   *requirements*. Examples use `127.0.0.1`, `user@your-server`, or `$SERVER`.
2. **Security honesty.** WebDAV is unauthenticated HTTP; QUIC has no client
   auth. Do not document public-internet deploy as safe. Prefer loopback
   examples; call out trusted-network assumptions.
3. **Fix durability at the source.** Never drop `sync_data` / `fsync` errors.
   WAL and block-store paths are load-bearing — see roadmap P0s.
4. **Keep the CLI and MCP at parity** for file ops (`ls`/`cat`/`stat`/`write`
   ↔ list/read/stat/write tools).
5. **Tests stay hermetic.** Bind servers to `127.0.0.1` with ephemeral or
   test-only ports; use `tempfile` for data dirs; no reliance on a developer’s
   remote box.
6. **Scope discipline.** Don’t expand into Garage/S3, full git UX, or auth
   frameworks unless the task names them — leave breadcrumbs in the roadmap.

## Useful entrypoints

| Task | Start here |
|------|------------|
| QUIC frames / message types | `crates/proxygit-common/src/protocol.rs` |
| CDC chunker | `crates/proxygit-common/src/cdc.rs` |
| Server dispatch + backups | `crates/proxygit-server/src/lib.rs` |
| WebDAV | `crates/proxygit-server/src/webdav.rs` |
| Block store fsync barrier | `crates/proxygit-server/src/block_store/mod.rs` |
| SQLite indexer | `crates/proxygit-server/src/indexer/mod.rs` |
| Client MCP tools | `crates/proxygit-client/src/lib.rs` |
| WAL | `crates/proxygit-client/src/wal/mod.rs` |
| CLI surface | `crates/proxygit-client/src/main.rs` |

## Optional local orchestration

Some maintainers drive parallel workers through external orchestrators. That is
**optional tooling**, not a repo dependency. The source of truth for merge
readiness remains the verification gate above. If you use an external runner,
keep its absolute paths and model names in your personal dotfiles — not in
committed docs.

## Security checklist for agents

Before exposing a server beyond localhost:

- [ ] `PROXYGIT_LISTEN` / `PROXYGIT_WEBDAV_LISTEN` bound appropriately
- [ ] Ports not published to `0.0.0.0` on a public interface
- [ ] Operator understands there is **no app-level auth** yet
- [ ] Sample manifests use placeholders, not real network identities
