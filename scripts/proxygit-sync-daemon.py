#!/usr/bin/env python3
"""
ProxyGit Bidirectional Sync Daemon

Syncs between flat-file directory (/root/proxygit-files/) and ProxyGit's
content-addressed block store (via WebDAV on localhost:3900).

Architecture:
  - Thread 1 (inotify_watcher): Watches flat file directory for changes via
    inotify. On IN_CLOSE_WRITE / IN_DELETE, debounces then PUT/DELETE to WebDAV.
  - Thread 2 (server_poller): Polls server file list every N seconds, downloads
    new/changed files to flat file directory.
  - SyncEngine: LWW conflict resolution, shared state.

Usage:
  proxygit-sync-daemon [OPTIONS]
    --once         Run one sync pass then exit (for testing/cron)
    --daemon       Run as daemon (default: auto-detect)
    --uuid <UUID>  Project UUID (default: 00000000-0000-0000-0000-000000000001)
    --webdav-url <URL>  WebDAV base URL (default: http://127.0.0.1:3900/webdav/)
    --flat-dir <PATH>   Flat file directory (default: /root/proxygit-files)
    --poll-interval <S> Server poll interval in seconds (default: 5)
    --debounce-ms <MS>  inotify debounce ms (default: 250)
    --log-dir <PATH>    Log directory (default: /var/log/proxygit-sync)
    --conflict-backup   Save overwritten files as .conflict.<ts>
"""

import argparse
import json
import logging
import os
import stat as stat_module
import sys
import threading
import time
import urllib.error
import urllib.request
from collections import defaultdict
from pathlib import Path

# ---------------------------------------------------------------------------
# Logging setup
# ---------------------------------------------------------------------------

logger = logging.getLogger("proxygit-sync")


def setup_logging(log_dir: str, daemon: bool):
    os.makedirs(log_dir, exist_ok=True)
    fmt = logging.Formatter(
        "%(asctime)s [%(levelname)s] %(message)s", datefmt="%Y-%m-%d %H:%M:%S"
    )
    fh = logging.FileHandler(os.path.join(log_dir, "sync.log"))
    fh.setFormatter(fmt)
    logger.addHandler(fh)
    if daemon:
        # In daemon mode, only log to file
        logger.setLevel(logging.INFO)
    else:
        # In --once mode, also log to stderr
        sh = logging.StreamHandler(sys.stderr)
        sh.setFormatter(fmt)
        logger.addHandler(sh)
        logger.setLevel(logging.DEBUG)


# ---------------------------------------------------------------------------
# WebDAV helpers
# ---------------------------------------------------------------------------

def webdav_list_files(webdav_url: str, project_uuid: str) -> list[dict]:
    """List all files in the project via WebDAV GET on root."""
    url = f"{webdav_url.rstrip('/')}/{project_uuid}/"
    req = urllib.request.Request(url, method="GET")
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            data = resp.read()
            return json.loads(data)
    except Exception as e:
        logger.warning(f"WebDAV list failed: {e}")
        return []


def webdav_get_file(webdav_url: str, project_uuid: str, path: str) -> bytes | None:
    """Download a file from the server via WebDAV GET."""
    url = f"{webdav_url.rstrip('/')}/{project_uuid}/{path}"
    req = urllib.request.Request(url, method="GET")
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return resp.read()
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return None
        logger.warning(f"WebDAV GET {path} failed: {e}")
        return None
    except Exception as e:
        logger.warning(f"WebDAV GET {path} failed: {e}")
        return None


def webdav_put_file(webdav_url: str, project_uuid: str, path: str, data: bytes):
    """Upload a file to the server via WebDAV PUT."""
    url = f"{webdav_url.rstrip('/')}/{project_uuid}/{path}"
    req = urllib.request.Request(url, data=data, method="PUT")
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            if resp.status in (200, 201, 204):
                logger.info(f"PUT {path} -> server (status {resp.status})")
                return True
            else:
                logger.warning(f"PUT {path} returned {resp.status}")
                return False
    except Exception as e:
        logger.warning(f"WebDAV PUT {path} failed: {e}")
        return False


def webdav_delete_file(webdav_url: str, project_uuid: str, path: str) -> bool:
    """Delete a file on the server via WebDAV DELETE."""
    url = f"{webdav_url.rstrip('/')}/{project_uuid}/{path}"
    req = urllib.request.Request(url, method="DELETE")
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            logger.info(f"DELETE {path} from server (status {resp.status})")
            return True
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return True  # Already gone
        logger.warning(f"WebDAV DELETE {path} failed: {e}")
        return False
    except Exception as e:
        logger.warning(f"WebDAV DELETE {path} failed: {e}")
        return False


# ---------------------------------------------------------------------------
# File system helpers
# ---------------------------------------------------------------------------

SKIP_PATTERNS = {
    ".DS_Store",
    ".localized",
    "._",
    ".svn",
    ".git",
}


def should_skip(name: str) -> bool:
    """Check if a filename should be skipped (metadata, temp files)."""
    if name in SKIP_PATTERNS:
        return True
    if name.startswith("._"):
        return True
    if name.startswith(".") and name.endswith(".swp"):
        return True
    if name.startswith(".") and name.endswith(".swo"):
        return True
    if name.endswith("~"):
        return True
    if name.endswith(".tmp"):
        return True
    return False


def is_visible_file(path: str) -> bool:
    """Check if a path is a regular file (not a dir, not a symlink, not skipped)."""
    name = os.path.basename(path)
    if should_skip(name):
        return False
    try:
        st = os.lstat(path)
        return stat_module.S_ISREG(st.st_mode)
    except OSError:
        return False


def is_directory(path: str) -> bool:
    """Check if a path is a directory."""
    try:
        st = os.lstat(path)
        return stat_module.S_ISDIR(st.st_mode)
    except OSError:
        return False


def get_file_mtime_size(path: str) -> tuple[int, int] | None:
    """Get (mtime_ns, size) for a file, or None if inaccessible."""
    try:
        st = os.stat(path)
        return (st.st_mtime_ns, st.st_size)
    except OSError:
        return None


def safe_mkdir(path: str):
    """Create directory if it doesn't exist."""
    os.makedirs(path, exist_ok=True)


def walk_flat_directory(flat_dir: str) -> dict[str, tuple[int, int]]:
    """Walk flat directory, return {relative_path: (mtime_ns, size)}."""
    result = {}
    flat_dir = os.path.abspath(flat_dir)
    for root, dirs, files in os.walk(flat_dir):
        # Filter out dot-directories
        dirs[:] = [d for d in dirs if not should_skip(d)]
        for fname in files:
            if should_skip(fname):
                continue
            full_path = os.path.join(root, fname)
            rel_path = os.path.relpath(full_path, flat_dir)
            mt_sz = get_file_mtime_size(full_path)
            if mt_sz is not None:
                result[rel_path] = mt_sz
    return result


# ---------------------------------------------------------------------------
# Sync Engine
# ---------------------------------------------------------------------------

class SyncEngine:
    """Handles conflict resolution and actual sync operations."""

    def __init__(
        self,
        webdav_url: str,
        project_uuid: str,
        flat_dir: str,
        conflict_backup: bool = False,
    ):
        self.webdav_url = webdav_url
        self.project_uuid = project_uuid
        self.flat_dir = flat_dir
        self.conflict_backup = conflict_backup
        # {path: (mtime_ns, size)}
        self.last_snapshot: dict[str, tuple[int, int]] = {}
        # {path: last_sync_timestamp_ns}
        self.last_sync_time: dict[str, int] = {}

    def load_snapshot(self):
        """Load initial snapshot from disk."""
        self.last_snapshot = walk_flat_directory(self.flat_dir)
        now = time.time_ns()
        for p in self.last_snapshot:
            self.last_sync_time[p] = now

    def sync_file_to_server(self, rel_path: str) -> bool:
        """Sync a single file from flat directory to server (SMB → Server)."""
        full_path = os.path.join(self.flat_dir, rel_path)
        if not os.path.isfile(full_path):
            logger.info(f"File {rel_path} no longer exists on disk, skipping")
            return False

        try:
            with open(full_path, "rb") as f:
                data = f.read()
        except OSError as e:
            logger.warning(f"Failed to read {rel_path}: {e}")
            return False

        success = webdav_put_file(self.webdav_url, self.project_uuid, rel_path, data)
        if success:
            mt_sz = get_file_mtime_size(full_path)
            if mt_sz:
                self.last_snapshot[rel_path] = mt_sz
                self.last_sync_time[rel_path] = time.time_ns()
        return success

    def delete_file_on_server(self, rel_path: str) -> bool:
        """Delete a file on the server."""
        success = webdav_delete_file(self.webdav_url, self.project_uuid, rel_path)
        if success:
            self.last_snapshot.pop(rel_path, None)
            self.last_sync_time.pop(rel_path, None)
        return success

    def sync_file_from_server(self, rel_path: str, server_entry: dict) -> bool:
        """Sync a file from server to flat directory (Server → SMB)."""
        data = webdav_get_file(self.webdav_url, self.project_uuid, rel_path)
        if data is None:
            logger.warning(f"Failed to download {rel_path} from server")
            return False

        full_path = os.path.join(self.flat_dir, rel_path)
        parent = os.path.dirname(full_path)
        safe_mkdir(parent)

        # If conflict backup is enabled and file exists locally
        if self.conflict_backup and os.path.exists(full_path):
            ts = int(time.time())
            backup_path = f"{full_path}.conflict.{ts}"
            try:
                os.rename(full_path, backup_path)
                logger.warning(f"CONFLICT: backed up {rel_path} to {backup_path}")
            except OSError:
                pass

        try:
            with open(full_path, "wb") as f:
                f.write(data)
            logger.info(f"Downloaded {rel_path} from server ({len(data)} bytes)")
            mt_sz = get_file_mtime_size(full_path)
            if mt_sz:
                self.last_snapshot[rel_path] = mt_sz
                self.last_sync_time[rel_path] = time.time_ns()
            return True
        except OSError as e:
            logger.warning(f"Failed to write {rel_path}: {e}")
            return False

    def delete_file_locally(self, rel_path: str):
        """Delete a file from the flat directory."""
        full_path = os.path.join(self.flat_dir, rel_path)
        try:
            os.remove(full_path)
            logger.info(f"Deleted local file {rel_path}")
            self.last_snapshot.pop(rel_path, None)
            self.last_sync_time.pop(rel_path, None)
        except OSError as e:
            logger.warning(f"Failed to delete {rel_path}: {e}")

    def resolve_and_sync(
        self, rel_path: str, smb_mtime_ns: int | None, server_entry: dict | None
    ):
        """
        Resolve conflict and sync.
        smb_mtime_ns: None if file doesn't exist on SMB side
        server_entry: None if file doesn't exist on server side
        """
        last_sync = self.last_sync_time.get(rel_path, 0)
        smb_exists = smb_mtime_ns is not None
        server_exists = server_entry is not None

        if not smb_exists and not server_exists:
            return  # Both sides agree: no file

        if smb_exists and not server_exists:
            # File exists on SMB but not on server → CREATE to server
            if smb_mtime_ns > last_sync:
                logger.info(f"SMB→Server: creating {rel_path}")
                self.sync_file_to_server(rel_path)
            else:
                # Already deleted on server, delete local
                logger.info(f"Server→SMB: deleting {rel_path} (server-side delete)")
                self.delete_file_locally(rel_path)
            return

        if server_exists and not smb_exists:
            # File exists on server but not on SMB → CREATE from server
            server_mtime = server_entry.get("mtime", 0) * 1_000_000_000  # sec to ns
            if server_mtime > last_sync:
                logger.info(f"Server→SMB: creating {rel_path}")
                self.sync_file_from_server(rel_path, server_entry)
            else:
                # Already deleted locally, delete from server
                logger.info(f"SMB→Server: deleting {rel_path} (local delete)")
                self.delete_file_on_server(rel_path)
            return

        # Both exist — check which changed
        server_mtime = server_entry.get("mtime", 0) * 1_000_000_000
        smb_changed = smb_mtime_ns > last_sync
        server_changed = server_mtime > last_sync

        if smb_changed and not server_changed:
            logger.info(f"SMB→Server: updating {rel_path} (SMB changed)")
            self.sync_file_to_server(rel_path)
        elif server_changed and not smb_changed:
            logger.info(f"Server→SMB: updating {rel_path} (server changed)")
            self.sync_file_from_server(rel_path, server_entry)
        elif smb_changed and server_changed:
            # TRUE CONFLICT — LWW
            if smb_mtime_ns >= server_mtime:
                logger.warning(
                    f"CONFLICT: {rel_path} — SMB won (LWW, mtime_ns={smb_mtime_ns} vs server={server_mtime})"
                )
                self.sync_file_to_server(rel_path)
            else:
                logger.warning(
                    f"CONFLICT: {rel_path} — Server won (LWW, mtime_ns={smb_mtime_ns} vs server={server_mtime})"
                )
                self.sync_file_from_server(rel_path, server_entry)
        else:
            # Neither changed since last sync
            logger.debug(f"No change for {rel_path}, skipping")

    def run_one_pass(self):
        """Run a single sync pass: compare server list with local snapshot, sync changes."""
        logger.info("Starting one-shot sync pass")

        # 1. List files on server
        server_files = webdav_list_files(self.webdav_url, self.project_uuid)
        server_map: dict[str, dict] = {}
        for entry in server_files:
            path = entry.get("path", "")
            if should_skip(os.path.basename(path)):
                continue
            server_map[path] = entry
        logger.info(f"Server has {len(server_map)} files")

        # 2. Walk local files
        local_map = walk_flat_directory(self.flat_dir)
        logger.info(f"Local has {len(local_map)} files")

        # 3. Collect all paths
        all_paths = set(server_map.keys()) | set(local_map.keys())
        logger.info(f"Total unique paths: {len(all_paths)}")

        # 4. Resolve and sync each
        for rel_path in sorted(all_paths):
            smb_mtime_ns = local_map.get(rel_path, (None, None))[0]
            server_entry = server_map.get(rel_path)
            self.resolve_and_sync(rel_path, smb_mtime_ns, server_entry)

        # 5. Update our snapshot
        self.last_snapshot = walk_flat_directory(self.flat_dir)
        now = time.time_ns()
        for p in self.last_snapshot:
            self.last_sync_time[p] = now

        logger.info("One-shot sync pass complete")


# ---------------------------------------------------------------------------
# Inotify Watcher Thread (SMB → Server)
# ---------------------------------------------------------------------------

class InotifyWatcher(threading.Thread):
    """Watch flat file directory for changes via inotify and sync to server."""

    def __init__(self, engine: SyncEngine, debounce_ms: int = 250):
        super().__init__(name="inotify-watcher", daemon=True)
        self.engine = engine
        self.debounce_ms = debounce_ms
        self._stop_event = threading.Event()
        # Pending changes: {path: timestamp}
        self._pending: dict[str, float] = {}
        self._lock = threading.Lock()

    def stop(self):
        self._stop_event.set()

    def run(self):
        try:
            from inotify_simple import INotify, flags
        except ImportError:
            logger.error(
                "inotify-simple not installed. Run: pip3 install inotify-simple"
            )
            return

        logger.info(f"Inotify watcher started on {self.engine.flat_dir}")

        inotify = INotify()
        wd_map: dict[int, str] = {}
        path_map: dict[str, int] = {}

        def add_watch(path: str):
            if os.path.isdir(path) and path not in path_map:
                wd = inotify.add_watch(
                    path,
                    flags.CLOSE_WRITE
                    | flags.CREATE
                    | flags.DELETE
                    | flags.MOVED_FROM
                    | flags.MOVED_TO,
                )
                wd_map[wd] = path
                path_map[path] = wd
                logger.debug(f"Watching {path} (wd={wd})")

        # Add watch on flat dir and subdirs
        add_watch(self.engine.flat_dir)
        for root, dirs, _ in os.walk(self.engine.flat_dir):
            dirs[:] = [d for d in dirs if not should_skip(d)]
            for d in dirs:
                add_watch(os.path.join(root, d))

        debounce_sec = self.debounce_ms / 1000.0

        while not self._stop_event.is_set():
            # Process pending events
            now = time.time()
            ready_paths = []
            with self._lock:
                for path, ts in list(self._pending.items()):
                    if now - ts >= debounce_sec:
                        ready_paths.append(path)
                        del self._pending[path]

            for rel_path in ready_paths:
                self._sync_changed_file(rel_path)

            # Read inotify events (with timeout so we check _stop_event)
            events = inotify.read(timeout=debounce_sec * 1000, read_delay=None)
            if events is None:
                continue

            for event in events:
                wd = event.wd
                if wd not in wd_map:
                    continue
                dir_path = wd_map[wd]
                filename = event.name
                if not filename or should_skip(filename):
                    continue

                full_path = os.path.join(dir_path, filename)
                rel_path = os.path.relpath(full_path, self.engine.flat_dir)

                # Handle new directories: add watch
                if flags.ISDIR & event.mask and (
                    flags.CREATE & event.mask or flags.MOVED_TO & event.mask
                ):
                    if not should_skip(filename):
                        add_watch(full_path)

                # Handle deletion inotify events
                if flags.DELETE & event.mask or flags.MOVED_FROM & event.mask:
                    # Sync deletion to server
                    if is_visible_file(full_path):
                        self._prepare_sync(rel_path)
                    elif flags.ISDIR & event.mask:
                        # Directory deleted — remove watch
                        if full_path in path_map:
                            old_wd = path_map.pop(full_path, None)
                            if old_wd is not None:
                                wd_map.pop(old_wd, None)
                            try:
                                inotify.rm_watch(old_wd)
                            except OSError:
                                pass
                        # Enqueue all files that were in this directory
                        # (We'll handle it in the next poll cycle)
                        pass

                # Handle close-write / create (file written)
                if flags.CLOSE_WRITE & event.mask or (
                    flags.CREATE & event.mask
                ):
                    if is_visible_file(full_path):
                        self._prepare_sync(rel_path)

            # Poll for directory structure changes (new subdirs) every cycle
            self._update_watches(inotify, wd_map, path_map)

        logger.info("Inotify watcher stopped")

    def _prepare_sync(self, rel_path: str):
        """Mark a path for sync after debounce."""
        with self._lock:
            self._pending[rel_path] = time.time()

    def _sync_changed_file(self, rel_path: str):
        """Sync a single changed file to server."""
        full_path = os.path.join(self.engine.flat_dir, rel_path)
        if os.path.isfile(full_path):
            logger.debug(f"inotify: syncing {rel_path} to server")
            self.engine.sync_file_to_server(rel_path)
        elif rel_path in self.engine.last_snapshot:
            # File was deleted
            logger.debug(f"inotify: deleting {rel_path} from server")
            self.engine.delete_file_on_server(rel_path)

    def _update_watches(self, inotify, wd_map, path_map):
        """Check for new subdirectories and add watches."""
        flat_dir = self.engine.flat_dir
        for root, dirs, _ in os.walk(flat_dir):
            dirs[:] = [d for d in dirs if not should_skip(d)]
            for d in dirs:
                full = os.path.join(root, d)
                if full not in path_map:
                    wd = inotify.add_watch(
                        full,
                        flags.CLOSE_WRITE
                        | flags.CREATE
                        | flags.DELETE
                        | flags.MOVED_FROM
                        | flags.MOVED_TO,
                    )
                    wd_map[wd] = full
                    path_map[full] = wd
                    logger.debug(f"Added watch on new dir {full} (wd={wd})")


# ---------------------------------------------------------------------------
# Server Poller Thread (Server → SMB)
# ---------------------------------------------------------------------------

class ServerPoller(threading.Thread):
    """Poll server file list and download new/changed files to flat directory."""

    def __init__(self, engine: SyncEngine, poll_interval: int = 5):
        super().__init__(name="server-poller", daemon=True)
        self.engine = engine
        self.poll_interval = poll_interval
        self._stop_event = threading.Event()

    def stop(self):
        self._stop_event.set()

    def run(self):
        logger.info(
            f"Server poller started (interval={self.poll_interval}s)"
        )
        while not self._stop_event.is_set():
            try:
                self._poll_once()
            except Exception as e:
                logger.error(f"Server poll error: {e}")
            self._stop_event.wait(self.poll_interval)

    def _poll_once(self):
        """Single poll: compare server file list with local."""
        server_files = webdav_list_files(
            self.engine.webdav_url, self.engine.project_uuid
        )
        if not server_files:
            return

        server_map: dict[str, dict] = {}
        for entry in server_files:
            path = entry.get("path", "")
            if should_skip(os.path.basename(path)):
                continue
            server_map[path] = entry

        local_map = walk_flat_directory(self.engine.flat_dir)
        all_paths = set(server_map.keys()) | set(local_map.keys())
        synced_count = 0

        for rel_path in sorted(all_paths):
            smb_mtime_ns = local_map.get(rel_path, (None, None))[0]
            server_entry = server_map.get(rel_path)
            last_sync = self.engine.last_sync_time.get(rel_path, 0)

            server_exists = server_entry is not None
            smb_exists = smb_mtime_ns is not None

            # Skip if neither changed since last sync
            server_mtime = (server_entry.get("mtime", 0) * 1_000_000_000) if server_exists else 0
            smb_changed = smb_exists and smb_mtime_ns > last_sync
            server_changed = server_exists and server_mtime > last_sync

            if not smb_changed and not server_changed:
                continue

            self.engine.resolve_and_sync(rel_path, smb_mtime_ns, server_entry)
            synced_count += 1

        if synced_count > 0:
            logger.info(f"Server poller synced {synced_count} changes")


# ---------------------------------------------------------------------------
# Main entry point
# ---------------------------------------------------------------------------

def parse_args(argv=None):
    parser = argparse.ArgumentParser(
        description="ProxyGit Bidirectional Sync Daemon"
    )
    parser.add_argument(
        "--once",
        action="store_true",
        help="Run one sync pass then exit",
    )
    parser.add_argument(
        "--daemon",
        action="store_true",
        help="Run as persistent daemon",
    )
    parser.add_argument(
        "--uuid",
        default="00000000-0000-0000-0000-000000000001",
        help="Project UUID (default: 00000000-0000-0000-0000-000000000001)",
    )
    parser.add_argument(
        "--webdav-url",
        default="http://127.0.0.1:3900/webdav/",
        help="WebDAV base URL (default: http://127.0.0.1:3900/webdav/)",
    )
    parser.add_argument(
        "--flat-dir",
        default="/root/proxygit-files",
        help="Flat file directory (default: /root/proxygit-files)",
    )
    parser.add_argument(
        "--poll-interval",
        type=int,
        default=5,
        help="Server poll interval in seconds (default: 5)",
    )
    parser.add_argument(
        "--debounce-ms",
        type=int,
        default=250,
        help="inotify debounce ms (default: 250)",
    )
    parser.add_argument(
        "--log-dir",
        default="/var/log/proxygit-sync",
        help="Log directory (default: /var/log/proxygit-sync)",
    )
    parser.add_argument(
        "--conflict-backup",
        action="store_true",
        help="Save overwritten files as .conflict.<ts>",
    )
    return parser.parse_args(argv)


def main():
    args = parse_args()
    setup_logging(args.log_dir, args.daemon)

    logger.info("ProxyGit Sync Daemon starting")
    logger.info(
        f"  WebDAV: {args.webdav_url}{args.uuid}/"
    )
    logger.info(f"  Flat dir: {args.flat_dir}")
    logger.info(f"  Poll interval: {args.poll_interval}s")
    logger.info(f"  Debounce: {args.debounce_ms}ms")

    if not os.path.isdir(args.flat_dir):
        logger.error(f"Flat directory {args.flat_dir} does not exist!")
        sys.exit(1)

    # Verify WebDAV connectivity
    try:
        resp = urllib.request.urlopen(
            urllib.request.Request(args.webdav_url), timeout=5
        )
        logger.info(f"WebDAV server reachable: {resp.status}")
    except Exception as e:
        logger.error(f"WebDAV server not reachable at {args.webdav_url}: {e}")
        if args.once:
            sys.exit(1)
        logger.warning("Continuing anyway (server may start later)")

    engine = SyncEngine(
        webdav_url=args.webdav_url,
        project_uuid=args.uuid,
        flat_dir=args.flat_dir,
        conflict_backup=args.conflict_backup,
    )
    engine.load_snapshot()
    logger.info(
        f"Initial snapshot: {len(engine.last_snapshot)} local files"
    )

    if args.once:
        engine.run_one_pass()
        return

    # Daemon mode
    watcher = InotifyWatcher(engine, debounce_ms=args.debounce_ms)
    poller = ServerPoller(engine, poll_interval=args.poll_interval)

    watcher.start()
    poller.start()

    logger.info("Daemon running. Press Ctrl+C to stop.")

    try:
        while True:
            time.sleep(1)
    except KeyboardInterrupt:
        logger.info("Shutting down...")
        watcher.stop()
        poller.stop()
        watcher.join(timeout=3)
        poller.join(timeout=3)
        logger.info("Shutdown complete")


if __name__ == "__main__":
    main()
