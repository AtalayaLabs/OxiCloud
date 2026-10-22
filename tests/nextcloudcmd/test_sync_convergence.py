"""nextcloudcmd ↔ OxiCloud sync convergence.

Each scenario changes the local tree, the server tree, or both, runs the
real Nextcloud sync engine, and requires both trees to end byte-identical
(`Pair.assert_converged`, which also requires an idle re-sync to be a no-op).
Where a scenario has a known right answer, the converged tree is also checked
against it, so "both sides agree on the wrong thing" — the silent-loss shape —
fails too.
"""

from __future__ import annotations

import os
import random
import time

from conftest import DIR, Local, Pair, Remote, Tree, sha256


def blob(tag: str, size: int = 256) -> bytes:
    """Deterministic, distinct content per tag."""
    seed = tag.encode()
    out = bytearray()
    while len(out) < size:
        seed = sha256(seed).encode()
        out += seed
    return bytes(out[:size])


def test_initial_sync_merges_both_sides(pair: Pair) -> None:
    pair.server.mkcol("from-server")
    pair.server.put("from-server/a.txt", blob("a"))
    pair.server.put("top.bin", blob("top", 70_000))
    pair.local.mkcol("from-local")
    pair.local.mkcol("from-local/nested")
    pair.local.put("from-local/nested/b.txt", blob("b"))
    pair.local.put("empty.txt", b"")

    pair.client.sync()

    pair.assert_converged({
        "from-server": DIR,
        "from-server/a.txt": sha256(blob("a")),
        "top.bin": sha256(blob("top", 70_000)),
        "from-local": DIR,
        "from-local/nested": DIR,
        "from-local/nested/b.txt": sha256(blob("b")),
        "empty.txt": sha256(b""),
    })


# Rename / move must leave the file's mtime and ETag alone
# (docs/plan/normalize-file-dates.md §1); otherwise the idle re-sync
# re-downloads the renamed file and re-uploads the files of the moved folder.
def test_local_rename_and_move_are_server_moves(pair: Pair) -> None:
    pair.local.mkcol("docs")
    pair.local.mkcol("docs/sub")
    pair.local.mkcol("archive")
    pair.local.put("docs/report.txt", blob("report"))
    pair.local.put("docs/sub/deep.txt", blob("deep"))
    pair.client.sync()
    file_id = pair.server.stat("docs/report.txt").fileid
    dir_id = pair.server.stat("docs/sub").fileid

    pair.local.move("docs/report.txt", "docs/report-final.txt")
    pair.local.move("docs/sub", "archive/sub")
    pair.client.sync()

    pair.assert_converged({
        "docs": DIR,
        "docs/report-final.txt": sha256(blob("report")),
        "archive": DIR,
        "archive/sub": DIR,
        "archive/sub/deep.txt": sha256(blob("deep")),
    })
    # A rename the client sent as MOVE keeps its identity. If it fell back to
    # delete + re-upload, shares, comments and versions on the file would be
    # lost with it.
    assert pair.server.stat("docs/report-final.txt").fileid == file_id
    assert pair.server.stat("archive/sub").fileid == dir_id


def test_server_rename_and_move_propagate(pair: Pair) -> None:
    pair.server.mkcol("inbox")
    pair.server.mkcol("inbox/batch")
    pair.server.mkcol("done")
    pair.server.put("inbox/batch/one.txt", blob("one"))
    pair.server.put("inbox/two.txt", blob("two"))
    pair.client.sync()

    pair.server.move("inbox/batch", "done/batch-1")
    pair.server.move("inbox/two.txt", "inbox/zwei.txt")
    pair.client.sync()

    pair.assert_converged({
        "inbox": DIR,
        "inbox/zwei.txt": sha256(blob("two")),
        "done": DIR,
        "done/batch-1": DIR,
        "done/batch-1/one.txt": sha256(blob("one")),
    })


def test_local_delete_goes_to_trash_and_restore_syncs_back(pair: Pair) -> None:
    pair.local.mkcol("project")
    pair.local.put("project/plan.txt", blob("plan"))
    pair.local.put("project/budget.ods", blob("budget", 50_000))
    pair.local.put("notes.txt", blob("notes"))
    pair.client.sync()
    before = pair.assert_converged()

    pair.local.delete("project")
    pair.local.delete("notes.txt")
    pair.client.sync()
    pair.assert_converged({})

    # The client's DELETE must land in the trash, not destroy the data.
    pair.server.restore("project")
    pair.server.restore("notes.txt")
    pair.client.sync()

    pair.assert_converged(before)


def test_server_delete_and_restore_round_trip(pair: Pair) -> None:
    pair.server.mkcol("photos")
    pair.server.put("photos/img.jpg", blob("img", 120_000))
    pair.client.sync()
    before = pair.assert_converged()

    pair.server.delete("photos")
    pair.client.sync()
    pair.assert_converged({})

    pair.server.restore("photos")
    pair.client.sync()
    pair.assert_converged(before)


def _contents(tree: Tree) -> set[str]:
    return {h for h in tree.values() if h != DIR}


def test_conflicting_edits_keep_both_versions(pair: Pair) -> None:
    pair.local.put("shared.txt", blob("base"))
    pair.client.sync()

    pair.local.put("shared.txt", blob("local-edit", 300))
    pair.server.put("shared.txt", blob("server-edit", 400))
    pair.client.sync()

    tree = pair.assert_converged()
    # Which version keeps the name is the client's policy; losing either is
    # not acceptable. The loser must survive as a conflict copy.
    assert {sha256(blob("local-edit", 300)), sha256(blob("server-edit", 400))} <= _contents(tree), tree
    assert any("conflict" in p.lower() for p in tree), tree


def test_local_edit_beats_server_delete(pair: Pair) -> None:
    pair.local.put("keep.txt", blob("v1"))
    pair.client.sync()

    pair.server.delete("keep.txt")
    pair.local.put("keep.txt", blob("v2", 300))
    pair.client.sync()

    # The unsynced local edit is the only copy of v2: it must be uploaded,
    # not deleted to match the server.
    pair.assert_converged({"keep.txt": sha256(blob("v2", 300))})


def test_server_edit_beats_local_delete(pair: Pair) -> None:
    pair.local.put("keep.txt", blob("v1"))
    pair.client.sync()

    pair.local.delete("keep.txt")
    pair.server.put("keep.txt", blob("v2", 300))
    pair.client.sync()

    pair.assert_converged({"keep.txt": sha256(blob("v2", 300))})


# ─────────────────────────────────────────────────────────────
# Interrupted syncs
#
# The transfer is throttled with --uplimit / --downlimit so the kill lands
# mid-transfer deterministically, rather than racing a fast loopback.
# ─────────────────────────────────────────────────────────────

UP_SIZE = 24 * 1024 * 1024  # 12 × 2 MB chunks (see SyncClient) → NC chunked upload v2
UP_LIMIT_KBPS = 2048  # ~12 s; the kill lands after a few seconds
# The server-side fixture is written with one plain PUT, so it has to stay
# under the test server's 4 MB OXICLOUD_DIRECT_PUT_MAX_BYTES.
DOWN_SIZE = 3_500_000
DOWN_LIMIT_KBPS = 256  # ~14 s


def test_interrupted_upload_never_exposes_a_partial_file(pair: Pair) -> None:
    data = os.urandom(UP_SIZE)
    pair.local.put("big.bin", data)
    pair.local.put("small.txt", blob("small"))

    pair.client.start(uplimit=UP_LIMIT_KBPS)
    time.sleep(4)
    rc = pair.client.kill()
    assert rc != 0, "sync finished before the kill landed — raise the size or lower the limit"

    # Mid-flight the server may lack big.bin, but must never serve a
    # truncated one: chunks only become a file at the final MOVE.
    mid = pair.server_tree()
    if "big.bin" in mid:
        assert mid["big.bin"] == sha256(data), "server exposed a partially uploaded big.bin"

    pair.client.sync()
    pair.assert_converged({"big.bin": sha256(data), "small.txt": sha256(blob("small"))})


def test_interrupted_download_resumes_to_identical_content(pair: Pair) -> None:
    data = os.urandom(DOWN_SIZE)
    pair.server.put("big.bin", data)

    pair.client.start(downlimit=DOWN_LIMIT_KBPS)
    time.sleep(4)
    rc = pair.client.kill()
    assert rc != 0, "sync finished before the kill landed — raise the size or lower the limit"

    # The client downloads into a dot-temp and renames on completion, so the
    # final name is either absent or complete.
    mid = pair.local_tree()
    if "big.bin" in mid:
        assert mid["big.bin"] == sha256(data), "client exposed a partially downloaded big.bin"

    pair.client.sync()
    pair.assert_converged({"big.bin": sha256(data)})


# ─────────────────────────────────────────────────────────────
# Randomised bidirectional churn against a model
#
# Each round hands every top-level folder to the local side, the server side,
# or neither, and applies random operations inside it. Sides never touch the
# same folder in one round, so the correct outcome is fully determined and
# kept in `model`; conflicts are covered by the deterministic tests above.
#
# Reproduce a failure with the seed printed in the assertion:
#   NCC_SEED=<seed> bash tests/nextcloudcmd/run.sh -k churn
# ─────────────────────────────────────────────────────────────

TOP_DIRS = [f"area{i}" for i in range(5)]
ROUNDS = int(os.environ.get("NCC_CHURN_ROUNDS", "8"))
OPS_PER_DIR = 4


class Churn:
    def __init__(self, rng: random.Random, model: Tree):
        self.rng = rng
        self.model = model
        self.counter = 0

    def name(self, prefix: str) -> str:
        self.counter += 1
        return f"{prefix}{self.counter}"

    def under(self, top: str, kind: str | None) -> list[str]:
        return sorted(
            p for p, h in self.model.items()
            if p.startswith(top + "/") and (kind is None or (h == DIR) == (kind == "dir"))
        )

    def dirs_in(self, top: str) -> list[str]:
        return [top, *self.under(top, "dir")]

    def op(self, side: Local | Remote, top: str) -> None:
        rng, model = self.rng, self.model
        files = self.under(top, "file")
        subdirs = self.under(top, "dir")
        choice = rng.choice(["create", "create", "modify", "rename", "move", "mkdir",
                             "rename_dir", "delete", "delete_dir"])

        if choice == "create" or (not files and choice in ("modify", "rename", "move", "delete")):
            path = f"{rng.choice(self.dirs_in(top))}/{self.name('f')}.txt"
            data = blob(path + str(self.counter), rng.randint(0, 5000))
            side.put(path, data)
            model[path] = sha256(data)
        elif choice == "modify":
            path = rng.choice(files)
            data = blob(path + str(self.counter), rng.randint(0, 5000))
            side.put(path, data)
            model[path] = sha256(data)
        elif choice in ("rename", "move"):
            src = rng.choice(files)
            parent = src.rsplit("/", 1)[0] if choice == "rename" else rng.choice(self.dirs_in(top))
            dst = f"{parent}/{self.name('r')}.txt"
            side.move(src, dst)
            model[dst] = model.pop(src)
        elif choice == "delete":
            path = rng.choice(files)
            side.delete(path)
            del model[path]
        elif choice == "mkdir" or not subdirs:
            path = f"{rng.choice(self.dirs_in(top))}/{self.name('d')}"
            side.mkcol(path)
            model[path] = DIR
        elif choice == "rename_dir":
            src = rng.choice(subdirs)
            dst = f"{src.rsplit('/', 1)[0]}/{self.name('d')}"
            side.move(src, dst)
            for p in [p for p in model if p == src or p.startswith(src + "/")]:
                model[dst + p[len(src):]] = model.pop(p)
        else:  # delete_dir
            path = rng.choice(subdirs)
            side.delete(path)
            for p in [p for p in model if p == path or p.startswith(path + "/")]:
                del model[p]


def test_bidirectional_churn_converges_to_model(pair: Pair) -> None:
    seed = int(os.environ.get("NCC_SEED", random.randrange(2**32)))
    rng = random.Random(seed)
    model: Tree = {}
    churn = Churn(rng, model)
    for top in TOP_DIRS:
        pair.server.mkcol(top)
        model[top] = DIR
    pair.client.sync()

    for rnd in range(ROUNDS):
        for top in TOP_DIRS:
            owner = rng.choice(["local", "server", None])
            if owner is None:
                continue
            side = pair.local if owner == "local" else pair.server
            for _ in range(OPS_PER_DIR):
                churn.op(side, top)
        pair.client.sync()
        try:
            pair.assert_converged(dict(model))
        except AssertionError as e:
            raise AssertionError(f"NCC_SEED={seed} round {rnd}: {e}") from None

