# ProxyGit — Agentic Workflow

Instructions for coding agents (and humans) working **in this repository**.
Runtime agents that *consume* a live ProxyGit mount should read the workspace
`.proxygit` manifest instead — see [`QUICKSTART.md`](QUICKSTART.md).

## Dispatch method

**Always use SC OMP, not raw CLI**, for multi-step implementation and review work
in this repo. Pair a main model with an advisor (`--advisor --slow`).

```bash
omp -e "$SC_OMP_HOOK" \
  --advisor --slow "$SC_OMP_ADVISOR" --model "$SC_OMP_MODEL" \
  "task"
```

Machine-local values (`SC_OMP_HOOK`, model ids, absolute hook paths) live in
**untracked** `AGENTS.local.md` (see `AGENTS.local.md.example`). Never commit
home-directory paths or personal model router ids.

### Worker configuration

| Role | Purpose |
|------|---------|
| **Builder** | Implementation + advisor review |
| **Reviewer** | Independent implementation pass + different advisor |

Concrete model ids for each role are defined in `AGENTS.local.md` (not upstream).

### Pair programming workflow

Use **2 parallel workers** for most tasks — one Builder-configured, one
Reviewer-configured. They converge results. Dispatch via the orchestrator’s
delegate/task mechanism.

**Phase implementation pattern:**

1. Orchestrator reads `ARCHITECTURE-ROADMAP.md` for priority
2. Splits work into 2 parallel dispatches (Builder worker, Reviewer worker)
3. Workers implement + test independently
4. Results converge in the orchestrator
5. Orchestrator runs the verification gate below

Single-worker is acceptable only for trivial doc-only or one-line fixes.

## Verification gate

Every change must pass before reporting done:

```bash
cargo check --release -p proxygit-server -p proxygit-client
TMPDIR=/tmp cargo test -p proxygit-server -p proxygit-client -p proxygit-common
cargo fmt --check
```

Smoke the path you touched (CLI verb, WebDAV, MCP, or server boot) — green
units alone are not enough for transport/IO changes.

## Key files

| File | Purpose |
|------|---------|
| `ARCHITECTURE-ROADMAP.md` | Priority matrix, phased implementation plan |
| `AGENTS.md` | This file — agentic workflow (committed policy) |
| `AGENTS.local.md` | Machine-local hook paths + model ids (**gitignored**) |
| `QUICKSTART.md` | User-facing deploy & mount guide |
| `SPEC.md` | Full system specification |
| `README.md` | Problem statement + share entrypoint |

## Architecture decisions

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
├── ARCHITECTURE-ROADMAP.md
├── AGENTS.md
├── QUICKSTART.md
├── SPEC.md
└── README.md
```

## Coding rules

1. **Follow the dispatch method above** for non-trivial work (SC OMP + dual workers).
2. **No environment lock-in in user-facing docs or committed defaults.**  
   Forbidden in README/QUICKSTART/examples: personal home paths, one-off lab
   hostnames, bare private-network IPs. Use `127.0.0.1`, `user@your-server`,
   `$SERVER`. Machine-local OMP paths belong only in `AGENTS.local.md`.
3. **Security honesty.** WebDAV is unauthenticated HTTP; QUIC has no client
   auth. Do not document public-internet deploy as safe. Prefer loopback
   examples; call out trusted-network assumptions.
4. **Fix durability at the source.** Never drop `sync_data` / `fsync` errors.
   WAL and block-store paths are load-bearing — see roadmap P0s.
5. **Keep the CLI and MCP at parity** for file ops (`ls`/`cat`/`stat`/`write`
   ↔ list/read/stat/write tools).
6. **Tests stay hermetic.** Bind servers to `127.0.0.1` with ephemeral or
   test-only ports; use `tempfile` for data dirs; no reliance on a developer’s
   remote box.
7. **Scope discipline.** Don’t expand into Garage/S3, full git UX, or auth
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

## `.proxygit` manifest (runtime workspaces)

Every ProxyGit-backed directory may contain a `.proxygit` file. Agents working
*on a mounted project* (not this source repo) should read it on startup:

```toml
[project]
name = "example"
uuid = "00000000-0000-0000-0000-000000000001"

[remote]
address = "127.0.0.1:8080"

[mcp]
address = "localhost:8082"
tools = ["read_file", "write_file", "list_directory", "stat", "get_project_map", "semantic_search"]

[webdav]
url = "http://127.0.0.1:3900/webdav/00000000-0000-0000-0000-000000000001/"
```

Agent flow: read `.proxygit` → `tools/list` on MCP → use file tools (prefer over
shelling out to the mount).

## Security checklist for agents

Before exposing a server beyond localhost:

- [ ] `PROXYGIT_LISTEN` / `PROXYGIT_WEBDAV_LISTEN` bound appropriately
- [ ] Ports not published to `0.0.0.0` on a public interface
- [ ] Operator understands there is **no app-level auth** yet
- [ ] Sample manifests use placeholders, not real network identities
