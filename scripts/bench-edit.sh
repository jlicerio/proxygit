#!/usr/bin/env bash
# bench-edit.sh — measure wire bytes for a 1KB edit in a ~1MB file
#
# Usage: ./scripts/bench-edit.sh [--quick]
#
# Prints greppable [wire] lines from client+server (RUST_LOG=proxygit::wire=info).
# Exit 0 only if the second WRITE_BLOCKS_SPARSE payload is well under the full file.

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

QUICK=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --quick) QUICK=true; shift ;;
    *) echo "Unknown arg: $1" >&2; exit 2 ;;
  esac
done

TARGET_DIR="${CARGO_TARGET_DIR:-$PROJECT_DIR/target}"
SERVER_BIN="$TARGET_DIR/release/proxygit-server"
CLIENT_BIN="$TARGET_DIR/release/proxygit-client"

if ! $QUICK || [[ ! -x "$SERVER_BIN" || ! -x "$CLIENT_BIN" ]]; then
  echo "=== Building proxygit (release) ==="
  cargo build --release -p proxygit-server -p proxygit-client
fi

DATA_DIR=$(mktemp -d /tmp/proxygit-bench-data.XXXXXX)
LOG_FILE=$(mktemp /tmp/proxygit-bench-log.XXXXXX)
WORK=$(mktemp -d /tmp/proxygit-bench-work.XXXXXX)

# Ephemeral ports to avoid clobbering a local lab server
QUIC_PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
DAV_PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
SERVER_ADDR="127.0.0.1:${QUIC_PORT}"
PROJECT="00000000-0000-0000-0000-000000000001"

cleanup() {
  if [[ -n "${SERVER_PID:-}" ]]; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$DATA_DIR" "$WORK"
  # keep LOG_FILE path printed for inspection
}
trap cleanup EXIT INT TERM

export PROXYGIT_DATA_DIR="$DATA_DIR"
export PROXYGIT_LISTEN="$SERVER_ADDR"
export PROXYGIT_WEBDAV_LISTEN="127.0.0.1:${DAV_PORT}"
export PROXYGIT_SERVER_CERT="$DATA_DIR/server_cert.der"
export RUST_LOG="proxygit::wire=info,proxygit_server=warn,proxygit_client=warn,info"

echo "=== Starting server on $SERVER_ADDR (logs → $LOG_FILE) ==="
"$SERVER_BIN" >"$LOG_FILE" 2>&1 &
SERVER_PID=$!

# Wait until cert exists (server wrote it) or timeout
for i in $(seq 1 40); do
  if [[ -f "$PROXYGIT_SERVER_CERT" ]]; then
    echo "Server ready after ~$((i * 100))ms"
    break
  fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "ERROR: server exited early" >&2
    cat "$LOG_FILE" >&2 || true
    exit 1
  fi
  sleep 0.1
done
if [[ ! -f "$PROXYGIT_SERVER_CERT" ]]; then
  echo "ERROR: server cert never appeared" >&2
  cat "$LOG_FILE" >&2 || true
  exit 1
fi

# ~1 MiB file: 1024 lines * 1024 bytes
python3 - <<'PY' >"$WORK/bigfile.txt"
line = (b"A" * 1023) + b"\n"
open(1, "wb").write(line * 1024)
PY
BYTES=$(wc -c <"$WORK/bigfile.txt" | tr -d ' ')
echo "Test file size: $BYTES bytes"

echo ""
echo "═══════════════════════════════════════"
echo "  TEST 1: initial write (~1 MiB)"
echo "═══════════════════════════════════════"
"$CLIENT_BIN" write "$SERVER_ADDR" "$PROJECT" "bigfile.txt" <"$WORK/bigfile.txt"

python3 - "$WORK/bigfile.txt" "$WORK/bigfile-edit.txt" <<'PY'
import sys
src, dst = sys.argv[1], sys.argv[2]
data = bytearray(open(src, "rb").read())
off = 512 * 1024
data[off : off + 1024] = b"Z" * 1024
open(dst, "wb").write(data)
print(f"patched 1024 bytes at offset {off}", file=sys.stderr)
PY

"$CLIENT_BIN" write "$SERVER_ADDR" "$PROJECT" "bigfile.txt" <"$WORK/bigfile-edit.txt"

# Give logs a moment to flush
sleep 0.2
kill "$SERVER_PID" 2>/dev/null || true
wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=

echo ""
echo "═══════════════════════════════════════"
echo "  Wire summary (client+server log)"
echo "═══════════════════════════════════════"
# Also capture client wire lines — client wrote to stderr of this script; re-run summary from LOG only for server.
# Client logs went to our stdout/stderr — tee by re-parsing nothing. Instead: client inherits RUST_LOG to stderr.
# We only redirected server. Re-extract from full transcript is hard; re-run client log via second approach:
# Parse both: server log file + any [wire] already printed above is mixed. Summarize server log + re-parse combined:

{
  cat "$LOG_FILE"
} | rg '\[wire\]' || true

echo ""
echo "--- Sparse WRITE payload sizes (recv WRITE_BLOCKS_SPARSE) ---"

python3 - "$LOG_FILE" "$BYTES" <<'PY'
import re, sys
log = open(sys.argv[1]).read()
file_size = int(sys.argv[2])
# server recv of sparse writes
pat = re.compile(
    r"\[wire\] recv type=0x14 \(WRITE_BLOCKS_SPARSE\) payload=(\d+) wire=(\d+)"
)
rows = [(int(a), int(b)) for a, b in pat.findall(log)]
print(f"WRITE_BLOCKS_SPARSE recv events: {len(rows)}")
for i, (payload, wire) in enumerate(rows, 1):
    print(f"  #{i}: payload={payload} wire={wire}  ({100*payload/file_size:.1f}% of file)")

if len(rows) < 2:
    print("FAIL: need initial full sparse write + edit sparse write")
    sys.exit(1)

first_p, first_w = rows[0]
second_p, second_w = rows[1]
# Initial write should be on the order of the file (plus hash overhead).
# Edit should be << file: one 64KiB block + per-block hash headers for all blocks.
if second_p >= file_size:
    print(f"FAIL: edit payload {second_p} not smaller than file {file_size}")
    sys.exit(1)
# A0/A2 gate: edit payload ≤ 128 KiB (one 64K block + headers for ~16 blocks is ~66K)
limit = 128 * 1024
if second_p > limit:
    print(f"FAIL: edit payload {second_p} > {limit} byte gate")
    sys.exit(1)
ratio = first_p / max(second_p, 1)
print(f"OK: edit/full payload ratio first/second = {ratio:.1f}x reduction on second write")
print(f"MEASURED: file={file_size} first_sparse_payload={first_p} edit_sparse_payload={second_p}")
PY

echo ""
echo "═══════════════════════════════════════"
echo "  Competitive baselines (same 1 KiB edit)"
echo "═══════════════════════════════════════"
# Baseline A: rsync --checksum whole-file replace (no rolling-hash delta without --inplace).
RSYNC_SRC="$WORK/rsync-src"
RSYNC_DST="$WORK/rsync-dst"
mkdir -p "$RSYNC_SRC" "$RSYNC_DST"
cp "$WORK/bigfile.txt" "$RSYNC_DST/bigfile.txt"
cp "$WORK/bigfile-edit.txt" "$RSYNC_SRC/bigfile.txt"
START_NS=$(python3 -c 'import time; print(time.time_ns())')
rsync -a --checksum "$RSYNC_SRC/bigfile.txt" "$RSYNC_DST/bigfile.txt"
END_NS=$(python3 -c 'import time; print(time.time_ns())')
RSYNC_MS=$(python3 -c "start=int('${START_NS}'); end=int('${END_NS}'); print(f'{(end-start)/1e6:.2f}')")
RSYNC_BYTES=$(wc -c <"$WORK/bigfile-edit.txt" | tr -d ' ')
echo "rsync --checksum wall_ms=$RSYNC_MS whole_file_bytes=$RSYNC_BYTES"

# Baseline B: cp whole-file local replace (lower bound for full rewrite).
CP_DST="$WORK/cp-dst"
mkdir -p "$CP_DST"
cp "$WORK/bigfile.txt" "$CP_DST/bigfile.txt"
START_NS=$(python3 -c 'import time; print(time.time_ns())')
cp "$WORK/bigfile-edit.txt" "$CP_DST/bigfile.txt"
END_NS=$(python3 -c 'import time; print(time.time_ns())')
CP_MS=$(python3 -c "start=int('${START_NS}'); end=int('${END_NS}'); print(f'{(end-start)/1e6:.2f}')")
echo "cp whole-file wall_ms=$CP_MS whole_file_bytes=$RSYNC_BYTES"

# Baseline C: scp loopback if available (still whole-file on the wire).
if command -v scp >/dev/null 2>&1 && [[ -f "$HOME/.ssh/id_ed25519" || -f "$HOME/.ssh/id_rsa" || -S "${SSH_AUTH_SOCK:-}" ]]; then
  SCP_DST="$WORK/scp-dst"
  mkdir -p "$SCP_DST"
  # Prefer loopback ssh; skip quietly if localhost ssh not usable.
  if ssh -o BatchMode=yes -o ConnectTimeout=2 -o StrictHostKeyChecking=no 127.0.0.1 true 2>/dev/null; then
    START_NS=$(python3 -c 'import time; print(time.time_ns())')
    scp -o BatchMode=yes -o StrictHostKeyChecking=no \
      "$WORK/bigfile-edit.txt" "127.0.0.1:$SCP_DST/bigfile.txt" >/dev/null 2>&1 || true
    END_NS=$(python3 -c 'import time; print(time.time_ns())')
    SCP_MS=$(python3 -c "start=int('${START_NS}'); end=int('${END_NS}'); print(f'{(end-start)/1e6:.2f}')")
    echo "scp loopback wall_ms=$SCP_MS whole_file_bytes=$RSYNC_BYTES"
  else
    echo "scp loopback SKIPPED (ssh 127.0.0.1 not available in BatchMode)"
  fi
else
  echo "scp SKIPPED (no scp or no ssh agent/key)"
fi

EDIT_PAYLOAD=$(python3 - "$LOG_FILE" <<'PY'
import re, sys
rows=[]
for line in open(sys.argv[1], errors="replace"):
    m=re.search(r"WRITE_BLOCKS_SPARSE.*payload_len=(\d+)", line)
    if m: rows.append(int(m.group(1)))
print(rows[1] if len(rows)>=2 else 0)
PY
)
echo ""
echo "SUMMARY TABLE (1 KiB mid-file edit of 1 MiB):"
printf "  %-22s %12s %12s\n" "method" "payload_B" "wall_ms"
printf "  %-22s %12s %12s\n" "----------------------" "------------" "------------"
printf "  %-22s %12s %12s\n" "proxygit sparse+zstd" "$EDIT_PAYLOAD" "(see log)"
printf "  %-22s %12s %12s\n" "rsync --checksum" "$RSYNC_BYTES" "$RSYNC_MS"
printf "  %-22s %12s %12s\n" "cp whole-file" "$RSYNC_BYTES" "$CP_MS"
echo "NOTE: ProxyGit sparse+zstd ships compressed changed blocks + hash list; rsync/cp/scp ship whole file contents."
echo "COMPARISON: proxygit_edit_payload<<rsync_whole_file_bytes"
echo "TIP: PROXYGIT_SPARSE_ZSTD=0 disables compression for A/B."

echo "Log kept at: $LOG_FILE"
# prevent cleanup from deleting log before user sees path — copy
cp "$LOG_FILE" /tmp/proxygit-bench-last.log
echo "Copy: /tmp/proxygit-bench-last.log"
