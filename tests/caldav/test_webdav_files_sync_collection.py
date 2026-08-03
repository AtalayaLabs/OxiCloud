"""RFC 6578 `sync-collection` REPORT — WebDAV files/folders, analogous to
`test_sync_collection.py`'s CalDAV/CardDAV coverage but for the original/
primary sync-collection surface (`storage.folder_sync_changes`).

`python-caldav` has no WebDAV-files support at all (CalDAV-only), so every
test here drives the server via raw HTTP through the same authenticated
`dav_client` session — the same precedent `test_carddav.py` established
for CardDAV in this suite.

WebDAV's actual member lifecycle differs from CalDAV/CardDAV in a way
that matters for these tests: a PUT-based "update" of an EXISTING file
reuses the SAME internal `member_id` (a real column UPDATE — see
`log_file_sync_changes_upd`'s migration comment), unlike CalDAV/CardDAV
where "update" is delete+recreate under a brand-new id. So the
single-row `DISTINCT ON (member_id)` collapse this suite's CalDAV churn
test could NOT actually exercise (delete+recreate mints two rows there)
is genuinely exercised here via a plain overwrite. (Trash+restore was
the original plan for this, but WebDAV's own DELETE
(`file_management_service.rs::delete_file`) is a genuine PERMANENT
delete, not a soft-delete/`is_trashed` flip — confirmed empirically —
so it doesn't reach the trash subsystem at all.) WebDAV also has real
MOVE and a heterogeneous folder+file member type, neither of which
CalDAV/CardDAV have at all.
"""

from __future__ import annotations

import re
import urllib.parse
import uuid
from concurrent.futures import ThreadPoolExecutor

import caldav
import requests


# ─────────────────────────────────────────────────────────────
# Helpers — mirror the pattern from test_sync_collection.py /
# test_carddav.py. Deliberately duplicated per this suite's
# established convention.
# ─────────────────────────────────────────────────────────────


def _put_file(dav_client: caldav.DAVClient, folder_url: str, name: str, body: bytes = b"webdav sync coverage") -> str:
    url = folder_url.rstrip("/") + f"/{name}"
    r = dav_client.request(url, method="PUT", body=body, headers={"Content-Type": "text/plain"})
    if r.status not in (200, 201, 204):
        raise AssertionError(f"PUT {url} → HTTP {r.status}\nresponse: {r.raw!r}")
    return url


def _delete(dav_client: caldav.DAVClient, url: str) -> int:
    r = dav_client.request(url, method="DELETE")
    return r.status


def _mkcol(dav_client: caldav.DAVClient, parent_url: str, name: str) -> str:
    url = parent_url.rstrip("/") + f"/{name}/"
    r = dav_client.request(url, method="MKCOL", body="")
    if r.status not in (200, 201):
        raise AssertionError(f"MKCOL {url} → HTTP {r.status}\n{r.raw!r}")
    return url


def _move(dav_client: caldav.DAVClient, src_url: str, dest_url: str) -> int:
    r = dav_client.request(src_url, method="MOVE", headers={"Destination": dest_url})
    return r.status


def _sync_collection_body(sync_token: str | None) -> str:
    token_el = (
        f"<D:sync-token>{sync_token}</D:sync-token>" if sync_token else "<D:sync-token/>"
    )
    return f"""<?xml version="1.0" encoding="utf-8"?>
<D:sync-collection xmlns:D="DAV:">
  {token_el}
  <D:prop>
    <D:getetag/>
    <D:resourcetype/>
  </D:prop>
</D:sync-collection>
"""


def _sync_collection(
    dav_client: caldav.DAVClient, collection_url: str, sync_token: str | None
) -> tuple[int, str]:
    r = dav_client.request(
        collection_url,
        method="REPORT",
        body=_sync_collection_body(sync_token),
        headers={"Content-Type": "application/xml; charset=utf-8", "Depth": "1"},
    )
    body = r.raw.decode("utf-8") if isinstance(r.raw, bytes) else r.raw
    return r.status, body


def _extract_sync_token(xml: str) -> str:
    m = re.search(r"<D:sync-token>(.*?)</D:sync-token>", xml)
    if not m:
        raise AssertionError(f"No non-empty <D:sync-token> in response:\n{xml}")
    return m.group(1)


def _response_blocks(xml: str) -> list[str]:
    return re.findall(r"<D:response>(.*?)</D:response>", xml, flags=re.DOTALL)


def _href_from_block(block: str) -> str:
    m = re.search(r"<D:href>(.*?)</D:href>", block)
    if not m:
        raise AssertionError(f"<D:response> block has no <D:href>:\n{block}")
    return m.group(1)


def _is_deleted_block(block: str) -> bool:
    return "<D:propstat>" not in block and "404" in block


def _is_collection_block(block: str, href: str) -> bool:
    """Folders render with a trailing-slash href and a
    `<D:resourcetype><D:collection/></D:resourcetype>` marker (standard
    WebDAV PROPFIND-shaped entry, reused for sync-collection upserts) —
    files have neither."""
    return href.rstrip().endswith("/") or "collection" in block.lower()


def _member_name_from_href(href: str) -> str:
    return urllib.parse.urlparse(href).path.rstrip("/").rsplit("/", 1)[-1]


def _apply_delta_to_mirror(mirror: set[str], xml: str) -> None:
    for block in _response_blocks(xml):
        href = _href_from_block(block)
        name = _member_name_from_href(href)
        if _is_deleted_block(block):
            mirror.discard(name)
        else:
            mirror.add(name)


# ─────────────────────────────────────────────────────────────
# Baseline protocol correctness
# ─────────────────────────────────────────────────────────────


def test_files_sync_initial_returns_full_listing_and_token(
    dav_client: caldav.DAVClient, fresh_webdav_folder: str
) -> None:
    name = f"sync-init-{uuid.uuid4().hex[:8]}.txt"
    _put_file(dav_client, fresh_webdav_folder, name)

    status, xml = _sync_collection(dav_client, fresh_webdav_folder, None)
    assert status == 207, f"Initial sync-collection → HTTP {status}\n{xml}"
    assert name in xml, f"Seeded file {name} missing from initial sync:\n{xml}"
    assert _extract_sync_token(xml)


def test_files_sync_noop_returns_empty_delta(
    dav_client: caldav.DAVClient, fresh_webdav_folder: str
) -> None:
    name = f"sync-noop-{uuid.uuid4().hex[:8]}.txt"
    _put_file(dav_client, fresh_webdav_folder, name)
    _, xml = _sync_collection(dav_client, fresh_webdav_folder, None)
    token = _extract_sync_token(xml)

    status, xml2 = _sync_collection(dav_client, fresh_webdav_folder, token)
    assert status == 207
    assert name not in xml2, f"No-op resync re-listed {name}:\n{xml2}"


def test_files_sync_delta_contains_only_new_files(
    dav_client: caldav.DAVClient, fresh_webdav_folder: str
) -> None:
    old_name = f"sync-old-{uuid.uuid4().hex[:8]}.txt"
    _put_file(dav_client, fresh_webdav_folder, old_name)
    _, xml = _sync_collection(dav_client, fresh_webdav_folder, None)
    token = _extract_sync_token(xml)

    new_name = f"sync-new-{uuid.uuid4().hex[:8]}.txt"
    _put_file(dav_client, fresh_webdav_folder, new_name)

    status, xml2 = _sync_collection(dav_client, fresh_webdav_folder, token)
    assert status == 207
    assert new_name in xml2, f"New file {new_name} missing from delta:\n{xml2}"
    assert old_name not in xml2, f"Untouched file {old_name} leaked into delta:\n{xml2}"


def test_files_sync_delete_reports_tombstone_and_disappears_from_query(
    dav_client: caldav.DAVClient, fresh_webdav_folder: str
) -> None:
    name = f"sync-del-{uuid.uuid4().hex[:8]}.txt"
    _put_file(dav_client, fresh_webdav_folder, name)
    _, xml = _sync_collection(dav_client, fresh_webdav_folder, None)
    token = _extract_sync_token(xml)

    assert _delete(dav_client, fresh_webdav_folder.rstrip("/") + f"/{name}") in (200, 204)

    status, xml2 = _sync_collection(dav_client, fresh_webdav_folder, token)
    assert status == 207
    blocks = [b for b in _response_blocks(xml2) if name in b]
    assert blocks, f"Deleted file {name} missing from delta entirely:\n{xml2}"
    assert _is_deleted_block(blocks[0]), (
        f"Deleted file {name}'s delta entry is not a tombstone:\n{blocks[0]}"
    )

    status3, xml3 = _sync_collection(dav_client, fresh_webdav_folder, None)
    assert status3 == 207
    assert name not in xml3, (
        f"File {name} still present in a fresh full listing after delete "
        f"— tombstone in the delta was a lie."
    )


def test_files_sync_token_rejected_against_wrong_folder(
    dav_client: caldav.DAVClient, fresh_webdav_folder: str, webdav_url: str
) -> None:
    other_name = f"pywebdav-{uuid.uuid4().hex[:12]}"
    other_url = webdav_url.rstrip("/") + f"/{other_name}/"
    r = dav_client.request(other_url, method="MKCOL", body="")
    assert r.status in (200, 201), f"MKCOL {other_url} → HTTP {r.status}"

    try:
        _, xml = _sync_collection(dav_client, fresh_webdav_folder, None)
        token = _extract_sync_token(xml)

        status, body = _sync_collection(dav_client, other_url, token)
        assert status == 400, (
            f"Cross-folder token replay expected HTTP 400; got {status}\n{body}"
        )
    finally:
        try:
            dav_client.request(other_url, method="DELETE")
        except Exception:
            pass


# ─────────────────────────────────────────────────────────────
# WebDAV-specific semantics
# ─────────────────────────────────────────────────────────────


def test_files_sync_overwrite_churn_nets_to_single_present_outcome(
    dav_client: caldav.DAVClient, fresh_webdav_folder: str
) -> None:
    """The genuine `DISTINCT ON (member_id)` collapse test: a PUT that
    overwrites an EXISTING WebDAV file is a real SQL UPDATE on the SAME
    row (`log_file_sync_changes_upd`), not delete+recreate — unlike
    WebDAV's own DELETE, which `file_management_service.rs::delete_file`
    makes a genuine PERMANENT delete (confirmed: WebDAV DELETE never
    trashes, so a trash+restore version of this test isn't reachable
    purely through the WebDAV surface). Repeated overwrites of the same
    href must collapse to exactly one delta entry — this is the one
    surface where that assertion holds without the server-side
    stale-tombstone-drop fix CalDAV/CardDAV's version of this test needs,
    because there's only ever one member_id involved here."""
    name = f"sync-overwrite-{uuid.uuid4().hex[:8]}.txt"
    _put_file(dav_client, fresh_webdav_folder, name, body=b"v1")
    _, xml = _sync_collection(dav_client, fresh_webdav_folder, None)
    token = _extract_sync_token(xml)

    _put_file(dav_client, fresh_webdav_folder, name, body=b"v2")
    _put_file(dav_client, fresh_webdav_folder, name, body=b"v3 final")

    status, xml2 = _sync_collection(dav_client, fresh_webdav_folder, token)
    assert status == 207
    blocks = [b for b in _response_blocks(xml2) if name in b]
    assert len(blocks) == 1, (
        f"Repeated overwrites of the same href must collapse to exactly "
        f"one delta entry; got {len(blocks)}:\n{xml2}"
    )
    assert not _is_deleted_block(blocks[0]), (
        f"Overwritten file {name} is still present but the delta "
        f"reports it deleted:\n{blocks[0]}"
    )


def test_files_sync_move_reflected_as_delete_from_old_create_in_new(
    dav_client: caldav.DAVClient, webdav_url: str
) -> None:
    folder_a = _mkcol(dav_client, webdav_url, f"sync-move-a-{uuid.uuid4().hex[:8]}")
    folder_b = _mkcol(dav_client, webdav_url, f"sync-move-b-{uuid.uuid4().hex[:8]}")
    try:
        name = f"movee-{uuid.uuid4().hex[:8]}.txt"
        src_url = _put_file(dav_client, folder_a, name)

        _, xml_a = _sync_collection(dav_client, folder_a, None)
        token_a = _extract_sync_token(xml_a)
        _, xml_b = _sync_collection(dav_client, folder_b, None)
        token_b = _extract_sync_token(xml_b)

        dest_url = folder_b.rstrip("/") + f"/{name}"
        move_status = _move(dav_client, src_url, dest_url)
        assert move_status in (200, 201, 204), f"MOVE → HTTP {move_status}"

        status_a, xml_a2 = _sync_collection(dav_client, folder_a, token_a)
        assert status_a == 207
        blocks_a = [b for b in _response_blocks(xml_a2) if name in b]
        assert blocks_a and _is_deleted_block(blocks_a[0]), (
            f"Source folder's delta must show {name} removed after "
            f"MOVE:\n{xml_a2}"
        )

        status_b, xml_b2 = _sync_collection(dav_client, folder_b, token_b)
        assert status_b == 207
        blocks_b = [b for b in _response_blocks(xml_b2) if name in b]
        assert blocks_b and not _is_deleted_block(blocks_b[0]), (
            f"Destination folder's delta must show {name} created after "
            f"MOVE:\n{xml_b2}"
        )
    finally:
        for url in (folder_a, folder_b):
            try:
                dav_client.request(url, method="DELETE")
            except Exception:
                pass


def test_folder_sync_delta_includes_subfolder_with_correct_member_type(
    dav_client: caldav.DAVClient, fresh_webdav_folder: str
) -> None:
    """The heterogeneous folder+file member_type dimension neither
    CalDAV nor CardDAV has — a created subfolder's delta entry must be
    distinguishable from a file's."""
    _, xml = _sync_collection(dav_client, fresh_webdav_folder, None)
    token = _extract_sync_token(xml)

    sub_name = f"sync-subfolder-{uuid.uuid4().hex[:8]}"
    _mkcol(dav_client, fresh_webdav_folder, sub_name)
    file_name = f"sync-plain-{uuid.uuid4().hex[:8]}.txt"
    _put_file(dav_client, fresh_webdav_folder, file_name)

    status, xml2 = _sync_collection(dav_client, fresh_webdav_folder, token)
    assert status == 207

    folder_blocks = [b for b in _response_blocks(xml2) if sub_name in b]
    file_blocks = [b for b in _response_blocks(xml2) if file_name in b]
    assert folder_blocks, f"Subfolder {sub_name} missing from delta:\n{xml2}"
    assert file_blocks, f"File {file_name} missing from delta:\n{xml2}"

    folder_href = _href_from_block(folder_blocks[0])
    file_href = _href_from_block(file_blocks[0])
    assert _is_collection_block(folder_blocks[0], folder_href), (
        f"Subfolder entry not distinguishable as a collection:\n{folder_blocks[0]}"
    )
    assert not _is_collection_block(file_blocks[0], file_href), (
        f"Plain file entry incorrectly looks like a collection:\n{file_blocks[0]}"
    )


# ─────────────────────────────────────────────────────────────
# Data-integrity stress tests
# ─────────────────────────────────────────────────────────────


def test_files_sync_local_mirror_matches_server_after_many_rounds(
    dav_client: caldav.DAVClient, fresh_webdav_folder: str
) -> None:
    """Same shape as the CalDAV/CardDAV local-mirror integrity tests."""
    mirror: set[str] = set()
    live_names: list[str] = []

    _, xml = _sync_collection(dav_client, fresh_webdav_folder, None)
    token = _extract_sync_token(xml)
    _apply_delta_to_mirror(mirror, xml)

    for round_no in range(5):
        for i in range(2):
            name = f"sync-mirror-r{round_no}-c{i}-{uuid.uuid4().hex[:6]}.txt"
            _put_file(dav_client, fresh_webdav_folder, name)
            live_names.append(name)

        if round_no % 3 == 2 and len(live_names) > 1:
            victim = live_names.pop(1)
            assert _delete(
                dav_client, fresh_webdav_folder.rstrip("/") + f"/{victim}"
            ) in (200, 204)

        status, xml = _sync_collection(dav_client, fresh_webdav_folder, token)
        assert status == 207, f"Round {round_no} resync → HTTP {status}\n{xml}"
        _apply_delta_to_mirror(mirror, xml)
        token = _extract_sync_token(xml)

    status_final, xml_final = _sync_collection(dav_client, fresh_webdav_folder, None)
    assert status_final == 207
    ground_truth = {
        _member_name_from_href(_href_from_block(b))
        for b in _response_blocks(xml_final)
    }
    assert mirror == ground_truth, (
        f"Local mirror built purely from sync-collection deltas diverged "
        f"from server ground truth.\n"
        f"Only in mirror (stale/phantom): {mirror - ground_truth}\n"
        f"Only on server (missed):        {ground_truth - mirror}"
    )


def test_files_sync_concurrent_writers_delta_contains_both(
    dav_client: caldav.DAVClient, fresh_webdav_folder: str
) -> None:
    _, xml = _sync_collection(dav_client, fresh_webdav_folder, None)
    token = _extract_sync_token(xml)

    name_a = f"sync-race-a-{uuid.uuid4().hex[:8]}.txt"
    name_b = f"sync-race-b-{uuid.uuid4().hex[:8]}.txt"

    with ThreadPoolExecutor(max_workers=2) as pool:
        fut_a = pool.submit(_put_file, dav_client, fresh_webdav_folder, name_a)
        fut_b = pool.submit(_put_file, dav_client, fresh_webdav_folder, name_b)
        fut_a.result()
        fut_b.result()

    status, xml2 = _sync_collection(dav_client, fresh_webdav_folder, token)
    assert status == 207
    assert name_a in xml2, f"Concurrently-written file {name_a} missing from delta:\n{xml2}"
    assert name_b in xml2, f"Concurrently-written file {name_b} missing from delta:\n{xml2}"


# ─────────────────────────────────────────────────────────────
# Retention / row-cap / expiry
# ─────────────────────────────────────────────────────────────


def test_files_sync_expired_token_after_row_cap_returns_507_then_recovers(
    dav_client: caldav.DAVClient,
    fresh_webdav_folder: str,
    base_url: str,
    admin_jwt: str,
) -> None:
    seed_name = f"sync-cap-seed-{uuid.uuid4().hex[:8]}.txt"
    _put_file(dav_client, fresh_webdav_folder, seed_name)
    _, xml = _sync_collection(dav_client, fresh_webdav_folder, None)
    stale_token = _extract_sync_token(xml)

    for i in range(10):
        name = f"sync-cap-churn-{i}-{uuid.uuid4().hex[:6]}.txt"
        _put_file(dav_client, fresh_webdav_folder, name)
        assert _delete(
            dav_client, fresh_webdav_folder.rstrip("/") + f"/{name}"
        ) in (200, 204)

    trigger_resp = requests.post(
        f"{base_url}/api/admin/jobs/sync_log_retention/trigger",
        params={"force": "true"},
        headers={"Authorization": f"Bearer {admin_jwt}"},
        timeout=30,
    )
    assert trigger_resp.status_code == 200, (
        f"Admin retention-job trigger → HTTP {trigger_resp.status_code}\n"
        f"{trigger_resp.text}"
    )
    outcome = trigger_resp.json()
    assert outcome.get("outcome", {}).get("outcome") == "ok", (
        f"Retention job did not report success: {outcome}"
    )

    status, body = _sync_collection(dav_client, fresh_webdav_folder, stale_token)
    assert status == 507, (
        f"Stale token after row-cap enforcement expected HTTP 507; got "
        f"{status}\n{body}"
    )

    status, xml2 = _sync_collection(dav_client, fresh_webdav_folder, None)
    assert status == 207, f"Recovery full resync → HTTP {status}\n{xml2}"
    assert seed_name in xml2, (
        f"Seed file {seed_name} (never deleted) missing from recovery "
        f"full resync:\n{xml2}"
    )
