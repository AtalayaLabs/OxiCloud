"""Shared harness for the nextcloudcmd sync-convergence suite.

Environment (injected by `run.sh`):
  OXICLOUD_BASE_URL      — server root, e.g. http://localhost:8092
  OXICLOUD_USERNAME      — admin username
  OXICLOUD_APP_PASSWORD  — app password (DAV surfaces refuse account passwords)
  NEXTCLOUDCMD_IMAGE     — tag of the image built from ./Dockerfile

Two actors touch every tree:

* the *local* side is a directory on disk that `nextcloudcmd` (the real
  Nextcloud desktop sync engine, run in Docker) reconciles with the server;
* the *server* side is a second client talking plain NC WebDAV
  (`/remote.php/dav/files/...`) — the stand-in for a web UI or another
  device editing the same account between syncs.

After a sync both views are reduced to `{relative path: sha256 | DIR}` and
compared. Every test works under its own remote folder (`--path`) and local
directory, so tests never see each other's files.
"""

from __future__ import annotations

import hashlib
import os
import re
import subprocess
import time
import uuid
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import quote, unquote, urlparse

import pytest
import requests

DIR = "<dir>"

DAV_NS = {"d": "DAV:", "oc": "http://owncloud.org/ns", "nc": "http://nextcloud.org/ns"}

PROPFIND_BODY = (
    '<?xml version="1.0"?>'
    '<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" '
    'xmlns:nc="http://nextcloud.org/ns"><d:prop>'
    "<d:resourcetype/><d:getetag/><oc:fileid/>"
    "<nc:trashbin-filename/><nc:trashbin-original-location/>"
    "</d:prop></d:propfind>"
)

Tree = dict[str, str]

# Folder ETags are bumped asynchronously: a write enqueues its ancestor
# chain and `TreeEtagFlushService` applies the bump within one
# OXICLOUD_TREE_ETAG_FLUSH_MS interval (see Folder::compute_etag). A sync
# started inside that window sees the new listing under the old ETag and
# may skip the folder until the next poll. Real clients poll every ~30 s,
# so every sync here first waits out two intervals — the suite tests
# convergence, not the width of that window.
SETTLE_S = 2 * int(os.environ.get("OXICLOUD_TREE_ETAG_FLUSH_MS", "500")) / 1000 + 0.2

# Client log markers of a transfer or remote mutation. An idle re-sync that
# logs any of them re-did work it had already done.
TRANSFER_JOB = re.compile(r"OCC::(PUTFileJob|GETFileJob|MoveJob|DeleteJob|MkColJob)\b")

# A DELETE the server answered 404 mutated nothing. nextcloudcmd 3.16.7 can
# leave a stale journal row when one sync both renames a folder and removes
# a child of it (it clears the row under the child's pre-rename path), and
# the next sync "propagates" that row as a remote delete. Seen with churn
# NCC_SEED=3421932573 round 4: idle DELETE area0/d46/f16.txt → 404, trees
# identical. Client-side, so the idle check tolerates exactly that case;
# a DELETE that removes something still fails it.
STALE_DELETE = re.compile(
    r'"DELETE" for QUrl\("[^"]*"\) FINISHED WITH STATUS "ContentNotFoundError'
)


def _idle_jobs(log: str) -> list[str]:
    """Transfer / mutation jobs in a sync log, minus DELETEs that hit a 404."""
    jobs = TRANSFER_JOB.findall(log)
    for _ in STALE_DELETE.finditer(log):
        if "DeleteJob" in jobs:
            jobs.remove("DeleteJob")
    return sorted(set(jobs))


def _env(name: str) -> str:
    value = os.environ.get(name)
    if not value:
        raise RuntimeError(
            f"Missing required env var {name}. Run this suite via "
            "tests/nextcloudcmd/run.sh (or `just test-nextcloudcmd`), which "
            "bootstraps the server, an app password and the client image."
        )
    return value


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


# ─────────────────────────────────────────────────────────────
# Server side — plain NC WebDAV
# ─────────────────────────────────────────────────────────────


@dataclass
class DavEntry:
    path: str  # relative to the PROPFIND target, no leading/trailing slash
    is_dir: bool
    etag: str | None
    fileid: str | None
    trash_filename: str | None
    trash_original_location: str | None


class Dav:
    def __init__(self, base_url: str, username: str, app_password: str):
        self.base_url = base_url.rstrip("/")
        self.username = username
        self.files_root = f"/remote.php/dav/files/{quote(username)}"
        self.trash_root = f"/remote.php/dav/trashbin/{quote(username)}/trash"
        self.http = requests.Session()
        self.http.auth = (username, app_password)

    def url(self, path: str) -> str:
        return f"{self.base_url}{self.files_root}/{quote(path.strip('/'))}"

    def _check(self, resp: requests.Response, *ok: int) -> requests.Response:
        if resp.status_code not in ok:
            raise AssertionError(
                f"{resp.request.method} {resp.request.url} → {resp.status_code}: {resp.text[:500]}"
            )
        return resp

    def put(self, path: str, data: bytes) -> None:
        self._check(self.http.put(self.url(path), data=data), 201, 204)

    def get(self, path: str) -> bytes:
        return self._check(self.http.get(self.url(path)), 200).content

    def mkcol(self, path: str) -> None:
        self._check(self.http.request("MKCOL", self.url(path)), 201)

    def move(self, src: str, dst: str) -> None:
        self._check(
            self.http.request(
                "MOVE",
                self.url(src),
                headers={"Destination": self.url(dst), "Overwrite": "F"},
            ),
            201,
            204,
        )

    def delete(self, path: str) -> None:
        self._check(self.http.delete(self.url(path)), 204)

    def exists(self, path: str) -> bool:
        resp = self.http.request("PROPFIND", self.url(path), headers={"Depth": "0"})
        return self._check(resp, 207, 404).status_code == 207

    def _propfind(self, url: str, prefix: str) -> list[DavEntry]:
        resp = self._check(
            self.http.request(
                "PROPFIND",
                url,
                headers={"Depth": "1", "Content-Type": "application/xml"},
                data=PROPFIND_BODY,
            ),
            207,
        )
        entries = []
        for r in ET.fromstring(resp.content).findall("d:response", DAV_NS):
            href = unquote(urlparse(r.findtext("d:href", "", DAV_NS)).path).rstrip("/")
            if not href.startswith(prefix):
                raise AssertionError(f"href {href!r} outside {prefix!r}")
            rel = href[len(prefix) :].strip("/")

            def prop(tag: str, r: ET.Element = r) -> str | None:
                # First propstat with a 200 carries the found props.
                for ps in r.findall("d:propstat", DAV_NS):
                    if "200" in ps.findtext("d:status", "", DAV_NS):
                        el = ps.find(f"d:prop/{tag}", DAV_NS)
                        if el is not None:
                            return el.text
                return None

            is_dir = r.find("d:propstat/d:prop/d:resourcetype/d:collection", DAV_NS) is not None
            entries.append(
                DavEntry(
                    path=rel,
                    is_dir=is_dir,
                    etag=prop("d:getetag"),
                    fileid=prop("oc:fileid"),
                    trash_filename=prop("nc:trashbin-filename"),
                    trash_original_location=prop("nc:trashbin-original-location"),
                )
            )
        return entries

    def list(self, path: str) -> list[DavEntry]:
        """Depth-1 children of `path` (the collection itself is dropped)."""
        prefix = unquote(urlparse(self.url(path)).path).rstrip("/")
        return [e for e in self._propfind(self.url(path), prefix) if e.path]

    def stat(self, path: str) -> DavEntry:
        prefix = unquote(urlparse(self.url(path)).path).rstrip("/")
        entries = self._propfind(self.url(path), prefix)
        return next(e for e in entries if e.path == "")

    def tree(self, root: str) -> Tree:
        """Recursive `{path: sha256 | DIR}` snapshot of `root`, by GETting every file.

        Walked with Depth: 1 PROPFINDs rather than Depth: infinity, which NC
        servers (and clients) treat as optional.
        """
        out: Tree = {}
        pending = [""]
        while pending:
            rel = pending.pop()
            for e in self.list(f"{root}/{rel}" if rel else root):
                child = f"{rel}/{e.path}" if rel else e.path
                if e.is_dir:
                    out[child] = DIR
                    pending.append(child)
                else:
                    out[child] = sha256(self.get(f"{root}/{child}"))
        return out

    def trash(self) -> list[DavEntry]:
        prefix = unquote(urlparse(self.base_url + self.trash_root).path)
        return [
            e
            for e in self._propfind(f"{self.base_url}{self.trash_root}/", prefix)
            if e.path
        ]

    def restore(self, original_location: str) -> None:
        """Restore the trash entry whose original location is `original_location`."""
        matches = [e for e in self.trash() if e.trash_original_location == original_location]
        assert len(matches) == 1, (
            f"expected one trash entry for {original_location!r}, got "
            f"{[e.trash_original_location for e in self.trash()]}"
        )
        # OxiCloud restores to the original location and ignores Destination
        # (pinned in tests/webdav/test_nc_move_copy_delete_trash.sh K2); the
        # header is sent anyway because the NC wire format requires it.
        resp = self.http.request(
            "MOVE",
            f"{self.base_url}{self.trash_root}/{matches[0].path}",
            headers={
                "Destination": f"{self.base_url}/remote.php/dav/trashbin/"
                f"{quote(self.username)}/restore/{matches[0].path}"
            },
        )
        self._check(resp, 201, 204)


class Remote:
    """`Dav` scoped to one remote folder, with the same verbs as `Local`.

    Lets a scenario say `side.move("a", "b")` without caring which side it
    is driving — the churn test picks the side per round.
    """

    def __init__(self, dav: Dav, root: str):
        self.dav = dav
        self.root = root

    def _p(self, path: str) -> str:
        return f"{self.root}/{path}"

    def put(self, path: str, data: bytes) -> None:
        self.dav.put(self._p(path), data)

    def get(self, path: str) -> bytes:
        return self.dav.get(self._p(path))

    def mkcol(self, path: str) -> None:
        self.dav.mkcol(self._p(path))

    def move(self, src: str, dst: str) -> None:
        self.dav.move(self._p(src), self._p(dst))

    def delete(self, path: str) -> None:
        self.dav.delete(self._p(path))

    def exists(self, path: str) -> bool:
        return self.dav.exists(self._p(path))

    def stat(self, path: str = "") -> DavEntry:
        return self.dav.stat(self._p(path) if path else self.root)

    def tree(self) -> Tree:
        return self.dav.tree(self.root)

    def restore(self, path: str) -> None:
        # trashbin-original-location is drive-relative, i.e. the same
        # shape as our paths under the user's files root.
        self.dav.restore(self._p(path))


# ─────────────────────────────────────────────────────────────
# Local side — directory reconciled by nextcloudcmd
# ─────────────────────────────────────────────────────────────


def local_tree(root: Path) -> Tree:
    """`{path: sha256 | DIR}` snapshot of a synced directory.

    Every dot-entry is skipped: that covers the client's own journal
    (`.sync_*.db*`), its log, and its in-flight download temporaries
    (`.name.~<hex>`). Tests never create dot-files, so nothing a test wrote
    is hidden by this.
    """
    out: Tree = {}
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if not d.startswith(".")]
        rel_dir = Path(dirpath).relative_to(root)
        for d in dirnames:
            out[(rel_dir / d).as_posix()] = DIR
        for f in filenames:
            if f.startswith("."):
                continue
            out[(rel_dir / f).as_posix()] = sha256((Path(dirpath) / f).read_bytes())
    return out


class Local:
    """Filesystem side. Every write pins an explicit, strictly increasing mtime.

    The sync engine detects local edits by (mtime, size); an edit landing in
    the same second as the previous sync with an unchanged size is
    indistinguishable from no edit. That is a client property, not a server
    one, so the harness rules it out instead of reporting it as a failure.
    """

    _clock = int(time.time()) - 100_000

    def __init__(self, root: Path):
        self.root = root

    def _tick(self, p: Path) -> None:
        Local._clock += 2
        os.utime(p, (Local._clock, Local._clock))

    def put(self, path: str, data: bytes) -> None:
        p = self.root / path
        p.write_bytes(data)
        self._tick(p)

    def get(self, path: str) -> bytes:
        return (self.root / path).read_bytes()

    def mkcol(self, path: str) -> None:
        (self.root / path).mkdir()

    def move(self, src: str, dst: str) -> None:
        assert not (self.root / dst).exists(), f"local move target {dst} exists"
        (self.root / src).rename(self.root / dst)

    def delete(self, path: str) -> None:
        p = self.root / path
        if p.is_dir():
            for child in sorted(p.rglob("*"), key=lambda c: len(c.parts), reverse=True):
                child.rmdir() if child.is_dir() else child.unlink()
            p.rmdir()
        else:
            p.unlink()

    def exists(self, path: str) -> bool:
        return (self.root / path).exists()


# ─────────────────────────────────────────────────────────────
# nextcloudcmd runner
# ─────────────────────────────────────────────────────────────


class SyncClient:
    def __init__(self, image: str, base_url: str, username: str, app_password: str,
                 local_dir: Path, remote_path: str, log_dir: Path):
        self.image = image
        self.base_url = base_url
        self.username = username
        self.app_password = app_password
        self.local_dir = local_dir
        self.remote_path = remote_path
        self.log_dir = log_dir
        self.runs = 0
        self._proc: subprocess.Popen[bytes] | None = None
        self._name: str | None = None

    def _cmd(self, name: str, uplimit: int | None, downlimit: int | None) -> list[str]:
        cmd = [
            "docker", "run", "--rm", "--name", name,
            "--network", "host",
            "--user", f"{os.getuid()}:{os.getgid()}",
            # nextcloudcmd writes a little config under $HOME; the mapped uid
            # has no passwd entry, so give it a writable one.
            "-e", "HOME=/tmp",
            # Conflict copies are kept local-only unless the server
            # advertises otherwise; force upload so a conflict converges to
            # identical trees and the "both versions survive" check can be
            # made on the server, where loss would actually matter.
            "-e", "OWNCLOUD_UPLOAD_CONFLICT_FILES=1",
            # Fixed 2 MB chunks. The client's own default (dynamic, starting
            # far above the test server's 4 MB OXICLOUD_DIRECT_PUT_MAX_BYTES /
            # OXICLOUD_CHUNK_MAX_BYTES caps) would send big files as one
            # PUT and get 413. Small fixed chunks also make the interrupted
            # upload land mid-way through NC chunked upload v2.
            "-e", "OWNCLOUD_CHUNK_SIZE=2000000",
            "-e", "OWNCLOUD_MIN_CHUNK_SIZE=1000000",
            "-e", "OWNCLOUD_MAX_CHUNK_SIZE=2000000",
            "-e", "OWNCLOUD_TARGET_CHUNK_UPLOAD_DURATION=0",
            "-v", f"{self.local_dir}:/sync",
            self.image,
            "--non-interactive",
            "--user", self.username,
            "--password", self.app_password,
            "--path", self.remote_path,
            "--max-sync-retries", "1",
        ]
        if uplimit is not None:
            cmd += ["--uplimit", str(uplimit)]
        if downlimit is not None:
            cmd += ["--downlimit", str(downlimit)]
        return cmd + ["/sync", self.base_url]

    def start(self, *, uplimit: int | None = None, downlimit: int | None = None) -> None:
        assert self._proc is None, "a sync is already running"
        time.sleep(SETTLE_S)
        self.runs += 1
        self._name = f"oxicloud-ncc-{uuid.uuid4().hex[:12]}"
        with (self.log_dir / f"sync-{self.runs:03d}.log").open("wb") as log:
            self._proc = subprocess.Popen(
                self._cmd(self._name, uplimit, downlimit), stdout=log, stderr=subprocess.STDOUT
            )

    def wait(self, timeout: float = 300) -> int:
        assert self._proc is not None
        try:
            return self._proc.wait(timeout=timeout)
        finally:
            self._proc = None

    def kill(self) -> int:
        """SIGKILL the running sync, mid-transfer — the 'laptop lid shut' case."""
        assert self._proc is not None and self._name is not None
        subprocess.run(["docker", "kill", self._name], capture_output=True, check=False)
        return self.wait(timeout=60)

    def log_text(self) -> str:
        return (self.log_dir / f"sync-{self.runs:03d}.log").read_text(errors="replace")

    def log_tail(self, lines: int = 40) -> str:
        return "\n".join(self.log_text().splitlines()[-lines:])

    def sync(self) -> None:
        self.start()
        rc = self.wait()
        assert rc == 0, f"nextcloudcmd exited {rc}; log tail:\n{self.log_tail()}"


# ─────────────────────────────────────────────────────────────
# Fixtures
# ─────────────────────────────────────────────────────────────


@dataclass
class Pair:
    """One synced pair: a local directory and the remote folder it mirrors."""

    client: SyncClient
    local: Local
    server: Remote

    def server_tree(self) -> Tree:
        return self.server.tree()

    def local_tree(self) -> Tree:
        return local_tree(self.local.root)

    def assert_converged(self, expected: Tree | None = None) -> Tree:
        """Local and server trees are identical (and equal `expected`, if given).

        A second sync must then be a no-op: no transfer or remote mutation
        in its log, and neither the remote root's ETag nor the local tree
        may move. A client that re-uploads or re-downloads on every run
        usually means an unstable ETag or mtime round-trip — cheap here, a
        storm of traffic at 10k users.
        """
        server = self.server_tree()
        local = self.local_tree()
        log = f"\nlast sync log ({self.client.log_dir}), tail:\n{self.client.log_tail()}"
        assert local == server, _tree_diff("local", local, "server", server) + log
        if expected is not None:
            assert server == expected, _tree_diff("server", server, "expected", expected) + log
        time.sleep(SETTLE_S)  # let this round's own ETag bumps land first
        etag = self.server.stat().etag
        self.client.sync()
        jobs = _idle_jobs(self.client.log_text())
        assert not jobs, f"an idle re-sync ran {jobs}; log: {self.client.log_dir}"
        assert self.server.stat().etag == etag, (
            "an idle re-sync changed the remote root ETag; log tail:\n"
            + self.client.log_tail()
        )
        assert self.local_tree() == local, "an idle re-sync changed the local tree"
        return server


def _tree_diff(an: str, a: Tree, bn: str, b: Tree) -> str:
    lines = [f"{an} != {bn}"]
    for path in sorted(a.keys() | b.keys()):
        if a.get(path) != b.get(path):
            lines.append(f"  {path}: {an}={a.get(path, '-')[:12]} {bn}={b.get(path, '-')[:12]}")
    return "\n".join(lines)


@pytest.fixture(scope="session")
def dav() -> Dav:
    return Dav(_env("OXICLOUD_BASE_URL"), _env("OXICLOUD_USERNAME"), _env("OXICLOUD_APP_PASSWORD"))


@pytest.fixture
def pair(dav: Dav, tmp_path: Path, request: pytest.FixtureRequest) -> Pair:
    remote = f"ncc-{request.node.name[:40]}-{uuid.uuid4().hex[:8]}"
    dav.mkcol(remote)
    local_dir = tmp_path / "local"
    local_dir.mkdir()
    log_dir = tmp_path / "logs"
    log_dir.mkdir()
    client = SyncClient(
        _env("NEXTCLOUDCMD_IMAGE"),
        _env("OXICLOUD_BASE_URL"),
        _env("OXICLOUD_USERNAME"),
        _env("OXICLOUD_APP_PASSWORD"),
        local_dir,
        f"/{remote}",
        log_dir,
    )
    return Pair(client=client, local=Local(local_dir), server=Remote(dav, remote))

