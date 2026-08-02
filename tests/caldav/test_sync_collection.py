"""RFC 6578 `sync-collection` REPORT — CalDAV events + CardDAV contacts,
driven through a real client session rather than hand-crafted Hurl XML.

Closes the gap `test_report.py` calls out explicitly (its own docstring,
"Not exercised here yet... Leave for later once real sync-token support
lands") — the durable per-collection change logs, per-collection
watermark, and retention/row-cap machinery have since landed
(`storage.folder_sync_changes`, `caldav.calendar_sync_changes`,
`carddav.contact_sync_changes`; `SyncCollectionEngine`;
`SyncLogRetentionService`).

`python-caldav` has no first-class `sync-collection` support (same gap
`test_report.py`'s multiget tests are already in), so every test here
issues the REPORT as raw XML via `calendar.client.request(...)` /
`dav_client.request(...)`, same escape hatch `_multiget_body` already
uses. CardDAV helpers are lifted from `test_carddav.py` (no client
library models CardDAV at all).

Focus: **data integrity**, not just protocol shape. Several tests build
an explicit local mirror from nothing but parsed deltas and check it
against the server's real ground-truth listing — the class of check that
would have caught the per-collection-watermark and `changes_since`
snapshot-race bugs fixed alongside this test file landing.
"""

from __future__ import annotations

import re
import textwrap
import urllib.parse
import uuid
from concurrent.futures import ThreadPoolExecutor

import caldav
import requests


# ─────────────────────────────────────────────────────────────
# Helpers — mirror the pattern from test_report.py / test_carddav.py.
# Deliberately duplicated for now (established convention in this
# suite); promote to conftest.py if a further file needs the same
# REPORT-building/parsing helpers.
# ─────────────────────────────────────────────────────────────


def _dedent_ical(body: str) -> str:
    return textwrap.dedent(body).strip().replace("\n", "\r\n") + "\r\n"


def _minimal_ical(uid: str, summary: str = "Sync coverage event") -> str:
    return _dedent_ical(
        f"""\
        BEGIN:VCALENDAR
        VERSION:2.0
        PRODID:-//pycaldav sync-collection coverage//EN
        BEGIN:VEVENT
        UID:{uid}
        DTSTAMP:20260101T080000Z
        DTSTART:20260101T090000Z
        DTEND:20260101T093000Z
        SUMMARY:{summary}
        END:VEVENT
        END:VCALENDAR
        """
    )


def _put_ical(calendar: caldav.Calendar, uid: str, body: str | None = None) -> None:
    url = str(calendar.url).rstrip("/") + f"/{uid}.ics"
    r = calendar.client.request(
        url,
        method="PUT",
        body=body if body is not None else _minimal_ical(uid),
        headers={"Content-Type": "text/calendar; charset=utf-8"},
    )
    if r.status < 200 or r.status >= 300:
        raise AssertionError(
            f"PUT {url} → HTTP {r.status}\nresponse: {r.raw!r}"
        )


def _delete_ical(calendar: caldav.Calendar, uid: str) -> int:
    url = str(calendar.url).rstrip("/") + f"/{uid}.ics"
    r = calendar.client.request(url, method="DELETE")
    return r.status


def _uid_from_event_data(data: str) -> str | None:
    for line in data.replace("\r\n", "\n").split("\n"):
        if line.startswith("UID:"):
            return line[4:].strip()
    return None


def _put_vcard(dav_client: caldav.DAVClient, addressbook_url: str, uid: str) -> None:
    url = addressbook_url.rstrip("/") + f"/{uid}.vcf"
    body = (
        f"BEGIN:VCARD\r\nVERSION:4.0\r\nUID:{uid}\r\n"
        f"FN:Sync Coverage Contact\r\nN:Coverage;Sync;;;\r\nEND:VCARD\r\n"
    )
    r = dav_client.request(
        url, method="PUT", body=body,
        headers={"Content-Type": "text/vcard; charset=utf-8"},
    )
    if r.status < 200 or r.status >= 300:
        raise AssertionError(f"PUT {url} → HTTP {r.status}\nresponse: {r.raw!r}")


def _delete_vcard(dav_client: caldav.DAVClient, addressbook_url: str, uid: str) -> int:
    url = addressbook_url.rstrip("/") + f"/{uid}.vcf"
    r = dav_client.request(url, method="DELETE")
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
  </D:prop>
</D:sync-collection>
"""


def _sync_collection(
    dav_client: caldav.DAVClient, collection_url: str, sync_token: str | None
) -> tuple[int, str]:
    """Issue a sync-collection REPORT. Returns (status, decoded body)."""
    r = dav_client.request(
        collection_url,
        method="REPORT",
        body=_sync_collection_body(sync_token),
        headers={"Content-Type": "application/xml; charset=utf-8"},
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
    """RFC 6578 §3.7 removed-member shape: `<D:status>...404...</D:status>`
    directly under `<D:response>`, with NO `<D:propstat>` — see
    `sync_collection_xml.rs::write_deleted_response`. An upserted member's
    block always has a `<D:propstat>` wrapping its own `<D:status>`."""
    return "<D:propstat>" not in block and "404" in block


def _member_id_from_href(href: str, suffix: str) -> str:
    """Last path segment, minus the `.ics`/`.vcf` suffix — the UID we
    minted, regardless of whether the server returned an absolute or
    relative href."""
    leaf = urllib.parse.urlparse(href).path.rstrip("/").rsplit("/", 1)[-1]
    if leaf.endswith(suffix):
        leaf = leaf[: -len(suffix)]
    return leaf


def _apply_delta_to_mirror(mirror: set[str], xml: str, suffix: str) -> None:
    """Mutates `mirror` in place from one sync-collection response's
    delta — the exact operation a real sync client performs on its local
    index. No ground-truth peeking here; this is purely delta-driven."""
    for block in _response_blocks(xml):
        member_id = _member_id_from_href(_href_from_block(block), suffix)
        if _is_deleted_block(block):
            mirror.discard(member_id)
        else:
            mirror.add(member_id)


# ─────────────────────────────────────────────────────────────
# Baseline protocol correctness — CalDAV
# ─────────────────────────────────────────────────────────────


def test_calendar_sync_initial_returns_full_listing_and_token(
    dav_client: caldav.DAVClient, fresh_calendar: caldav.Calendar
) -> None:
    """Empty sync-token → 207, the seeded event present, a non-empty
    sync-token returned — the RFC 6578 §3.7 baseline every subsequent
    test in this file builds on."""
    uid = f"sync-init-{uuid.uuid4().hex[:8]}"
    _put_ical(fresh_calendar, uid)

    status, xml = _sync_collection(dav_client, str(fresh_calendar.url), None)
    assert status == 207, f"Initial sync-collection → HTTP {status}\n{xml}"
    assert uid in xml, f"Seeded event {uid} missing from initial sync:\n{xml}"
    token = _extract_sync_token(xml)
    assert token, "Initial sync-collection must return a non-empty sync-token"


def test_calendar_sync_noop_returns_empty_delta(
    dav_client: caldav.DAVClient, fresh_calendar: caldav.Calendar
) -> None:
    """Resyncing with the latest token and no intervening changes returns
    an empty delta — real incremental sync, not a full re-list every
    poll."""
    uid = f"sync-noop-{uuid.uuid4().hex[:8]}"
    _put_ical(fresh_calendar, uid)
    _, xml = _sync_collection(dav_client, str(fresh_calendar.url), None)
    token = _extract_sync_token(xml)

    status, xml2 = _sync_collection(dav_client, str(fresh_calendar.url), token)
    assert status == 207
    assert uid not in xml2, (
        f"No-op resync must not re-list the untouched event {uid}:\n{xml2}"
    )


def test_calendar_sync_delta_contains_only_new_events(
    dav_client: caldav.DAVClient, fresh_calendar: caldav.Calendar
) -> None:
    """Create a second event after the baseline token; resync with that
    token → delta is exactly the new event, not the untouched one."""
    old_uid = f"sync-old-{uuid.uuid4().hex[:8]}"
    _put_ical(fresh_calendar, old_uid)
    _, xml = _sync_collection(dav_client, str(fresh_calendar.url), None)
    token = _extract_sync_token(xml)

    new_uid = f"sync-new-{uuid.uuid4().hex[:8]}"
    _put_ical(fresh_calendar, new_uid)

    status, xml2 = _sync_collection(dav_client, str(fresh_calendar.url), token)
    assert status == 207
    assert new_uid in xml2, f"New event {new_uid} missing from delta:\n{xml2}"
    assert old_uid not in xml2, (
        f"Untouched event {old_uid} leaked into delta:\n{xml2}"
    )


def test_calendar_sync_delete_reports_tombstone_and_disappears_from_query(
    dav_client: caldav.DAVClient, fresh_calendar: caldav.Calendar
) -> None:
    """Delete an event, resync with the pre-delete token → the delta
    carries an RFC 6578 §3.7 404 sub-response for it, AND a plain
    calendar-query no longer returns it. Cross-checks the delta's claim
    against ground truth rather than trusting the delta alone."""
    uid = f"sync-del-{uuid.uuid4().hex[:8]}"
    _put_ical(fresh_calendar, uid)
    _, xml = _sync_collection(dav_client, str(fresh_calendar.url), None)
    token = _extract_sync_token(xml)

    assert _delete_ical(fresh_calendar, uid) in (200, 204)

    status, xml2 = _sync_collection(dav_client, str(fresh_calendar.url), token)
    assert status == 207
    blocks = [b for b in _response_blocks(xml2) if uid in b]
    assert blocks, f"Deleted event {uid} missing from delta entirely:\n{xml2}"
    assert _is_deleted_block(blocks[0]), (
        f"Deleted event {uid}'s delta entry is not a tombstone "
        f"(no bare 404 status):\n{blocks[0]}"
    )

    ground_truth_uids = {
        _uid_from_event_data(e.data) for e in fresh_calendar.events()
    }
    assert uid not in ground_truth_uids, (
        f"Event {uid} still present in ground-truth listing after delete "
        f"— tombstone in the delta was a lie."
    )


def test_calendar_sync_token_rejected_against_wrong_calendar(
    dav_client: caldav.DAVClient,
    fresh_calendar: caldav.Calendar,
) -> None:
    """A sync-token minted on calendar A, replayed against calendar B,
    is rejected — pins `SyncToken::parse_for_collection`'s
    collision check (a token embeds its own collection id)."""
    principal = dav_client.principal()
    other_name = f"pycaldav-{uuid.uuid4().hex[:12]}"
    principal.make_calendar(name=other_name)
    other_calendar = next(
        (c for c in principal.calendars() if c.get_display_name() == other_name),
        None,
    )
    assert other_calendar is not None, "Second calendar failed to provision"

    try:
        _, xml = _sync_collection(dav_client, str(fresh_calendar.url), None)
        token = _extract_sync_token(xml)

        status, body = _sync_collection(dav_client, str(other_calendar.url), token)
        assert status == 400, (
            f"Cross-calendar token replay expected HTTP 400; got {status}\n{body}"
        )
    finally:
        try:
            other_calendar.delete()
        except Exception:
            pass


# ─────────────────────────────────────────────────────────────
# Baseline protocol correctness — CardDAV
# ─────────────────────────────────────────────────────────────


def test_contact_sync_delta_contains_only_new_contacts(
    dav_client: caldav.DAVClient, fresh_addressbook: str
) -> None:
    old_uid = f"sync-old-{uuid.uuid4().hex[:8]}"
    _put_vcard(dav_client, fresh_addressbook, old_uid)
    _, xml = _sync_collection(dav_client, fresh_addressbook, None)
    token = _extract_sync_token(xml)

    new_uid = f"sync-new-{uuid.uuid4().hex[:8]}"
    _put_vcard(dav_client, fresh_addressbook, new_uid)

    status, xml2 = _sync_collection(dav_client, fresh_addressbook, token)
    assert status == 207
    assert new_uid in xml2, f"New contact {new_uid} missing from delta:\n{xml2}"
    assert old_uid not in xml2, (
        f"Untouched contact {old_uid} leaked into delta:\n{xml2}"
    )


def test_contact_sync_delete_reports_tombstone(
    dav_client: caldav.DAVClient, fresh_addressbook: str
) -> None:
    uid = f"sync-del-{uuid.uuid4().hex[:8]}"
    _put_vcard(dav_client, fresh_addressbook, uid)
    _, xml = _sync_collection(dav_client, fresh_addressbook, None)
    token = _extract_sync_token(xml)

    assert _delete_vcard(dav_client, fresh_addressbook, uid) in (200, 204)

    status, xml2 = _sync_collection(dav_client, fresh_addressbook, token)
    assert status == 207
    blocks = [b for b in _response_blocks(xml2) if uid in b]
    assert blocks, f"Deleted contact {uid} missing from delta entirely:\n{xml2}"
    assert _is_deleted_block(blocks[0]), (
        f"Deleted contact {uid}'s delta entry is not a tombstone:\n{blocks[0]}"
    )


def test_contact_sync_token_rejected_against_wrong_addressbook(
    dav_client: caldav.DAVClient,
    fresh_addressbook: str,
    carddav_url: str,
) -> None:
    other_name = f"pycarddav-{uuid.uuid4().hex[:12]}"
    other_url = carddav_url.rstrip("/") + f"/{other_name}/"
    r = dav_client.request(other_url, method="MKCOL", body="")
    assert r.status in (200, 201), f"MKCOL {other_url} → HTTP {r.status}"

    try:
        _, xml = _sync_collection(dav_client, fresh_addressbook, None)
        token = _extract_sync_token(xml)

        status, body = _sync_collection(dav_client, other_url, token)
        assert status == 400, (
            f"Cross-addressbook token replay expected HTTP 400; got "
            f"{status}\n{body}"
        )
    finally:
        try:
            dav_client.request(other_url, method="DELETE")
        except Exception:
            pass


# ─────────────────────────────────────────────────────────────
# Data-integrity stress tests
# ─────────────────────────────────────────────────────────────


def test_calendar_sync_churn_nets_to_single_correct_outcome(
    dav_client: caldav.DAVClient, fresh_calendar: caldav.Calendar
) -> None:
    """CalDAV PUT-based 'update' is delete+recreate at the application
    layer (a NEW internal member row, not a SQL UPDATE of the old one —
    see calendar_sync_collection.hurl's note), so deleting then
    recreating the SAME href within one poll window produces two
    genuinely different underlying rows: a tombstone for the original
    member_id and a creation for the new one, both rendering to the
    same href. The server must resolve this to a SINGLE delta entry
    (the recreation) before rendering — the response format buckets
    upserts and deletions into separate lists with no ordering between
    them (`split_homogeneous`), so a client has no way to reconcile a
    same-href tombstone + upsert pair itself if the server hands both
    over. A stale tombstone reaching the client here would be a real
    correctness bug: the client would delete a resource that actually
    still exists."""
    uid = f"sync-churn-{uuid.uuid4().hex[:8]}"
    _, xml = _sync_collection(dav_client, str(fresh_calendar.url), None)
    token = _extract_sync_token(xml)

    _put_ical(fresh_calendar, uid)
    _delete_ical(fresh_calendar, uid)
    _put_ical(fresh_calendar, uid, _minimal_ical(uid, summary="Final state"))

    status, xml2 = _sync_collection(dav_client, str(fresh_calendar.url), token)
    assert status == 207
    blocks = [b for b in _response_blocks(xml2) if uid in b]
    assert len(blocks) == 1, (
        f"Delete+recreate of the same href within one poll window must "
        f"net to exactly one delta entry (the recreation) — a stale "
        f"tombstone for the same href must be dropped server-side. Got "
        f"{len(blocks)}:\n{xml2}"
    )
    assert not _is_deleted_block(blocks[0]), (
        f"Churned event {uid}'s true final state is 'present' "
        f"(recreated last) but the delta reports it deleted — a client "
        f"applying this would incorrectly delete a resource that still "
        f"exists:\n{blocks[0]}"
    )


def test_calendar_sync_local_mirror_matches_server_after_many_rounds(
    dav_client: caldav.DAVClient, fresh_calendar: caldav.Calendar
) -> None:
    """The core data-integrity check: a client that does nothing but
    apply successive sync-collection deltas to a local set must end up
    with EXACTLY the server's real member set — no stale entries left
    behind (would indicate a missed deletion, e.g. the watermark
    false-expiry bug), no missing entries (would indicate a lost
    creation, e.g. the changes_since snapshot race).

    5 rounds, each mixing creates/updates/deletes across a handful of
    events; the local mirror is updated purely from parsed deltas,
    never by peeking at server state mid-run."""
    collection_url = str(fresh_calendar.url)
    mirror: set[str] = set()
    live_uids: list[str] = []

    _, xml = _sync_collection(dav_client, collection_url, None)
    token = _extract_sync_token(xml)
    _apply_delta_to_mirror(mirror, xml, ".ics")

    for round_no in range(5):
        # Create two new events every round.
        for i in range(2):
            uid = f"sync-mirror-r{round_no}-c{i}-{uuid.uuid4().hex[:6]}"
            _put_ical(fresh_calendar, uid)
            live_uids.append(uid)

        # Update (delete+recreate — the only "update" a real CalDAV
        # client surface produces, per calendar_sync_collection.hurl's
        # note) the oldest still-live event, every other round.
        if round_no % 2 == 1 and live_uids:
            target = live_uids[0]
            _delete_ical(fresh_calendar, target)
            _put_ical(fresh_calendar, target, _minimal_ical(target, summary="Updated"))

        # Delete the second-oldest still-live event, every third round.
        if round_no % 3 == 2 and len(live_uids) > 1:
            victim = live_uids.pop(1)
            _delete_ical(fresh_calendar, victim)

        status, xml = _sync_collection(dav_client, collection_url, token)
        assert status == 207, f"Round {round_no} resync → HTTP {status}\n{xml}"
        _apply_delta_to_mirror(mirror, xml, ".ics")
        token = _extract_sync_token(xml)

    ground_truth = {
        _uid_from_event_data(e.data) for e in fresh_calendar.events()
    }
    assert mirror == ground_truth, (
        f"Local mirror built purely from sync-collection deltas diverged "
        f"from server ground truth after {5} rounds.\n"
        f"Only in mirror (stale/phantom): {mirror - ground_truth}\n"
        f"Only on server (missed):        {ground_truth - mirror}"
    )


def test_contact_sync_local_mirror_matches_server_after_many_rounds(
    dav_client: caldav.DAVClient, fresh_addressbook: str
) -> None:
    """CardDAV counterpart of the CalDAV local-mirror integrity test —
    same engine (`SyncCollectionEngine`), same invariant."""
    mirror: set[str] = set()
    live_uids: list[str] = []

    _, xml = _sync_collection(dav_client, fresh_addressbook, None)
    token = _extract_sync_token(xml)
    _apply_delta_to_mirror(mirror, xml, ".vcf")

    for round_no in range(5):
        for i in range(2):
            uid = f"sync-mirror-r{round_no}-c{i}-{uuid.uuid4().hex[:6]}"
            _put_vcard(dav_client, fresh_addressbook, uid)
            live_uids.append(uid)

        if round_no % 3 == 2 and len(live_uids) > 1:
            victim = live_uids.pop(1)
            _delete_vcard(dav_client, fresh_addressbook, victim)

        status, xml = _sync_collection(dav_client, fresh_addressbook, token)
        assert status == 207, f"Round {round_no} resync → HTTP {status}\n{xml}"
        _apply_delta_to_mirror(mirror, xml, ".vcf")
        token = _extract_sync_token(xml)

    # Ground truth via PROPFIND Depth 1 on the address book — same
    # discovery technique `fresh_addressbook` itself uses.
    r = dav_client.request(
        fresh_addressbook,
        method="PROPFIND",
        body=(
            '<?xml version="1.0" encoding="UTF-8"?>'
            '<D:propfind xmlns:D="DAV:"><D:prop><D:displayname/></D:prop></D:propfind>'
        ),
        headers={"Depth": "1", "Content-Type": "application/xml"},
    )
    assert 200 <= r.status < 300
    xml_body = r.raw.decode("utf-8") if isinstance(r.raw, bytes) else r.raw
    ground_truth = {
        _member_id_from_href(href, ".vcf")
        for href in re.findall(r"<D:href>(.*?)</D:href>", xml_body)
        if href.endswith(".vcf")
    }
    assert mirror == ground_truth, (
        f"Local mirror diverged from server ground truth.\n"
        f"Only in mirror (stale/phantom): {mirror - ground_truth}\n"
        f"Only on server (missed):        {ground_truth - mirror}"
    )


def test_calendar_sync_concurrent_writers_delta_contains_both(
    dav_client: caldav.DAVClient, fresh_calendar: caldav.Calendar
) -> None:
    """Two near-simultaneous PUTs into the same calendar, then a resync
    with the pre-write token, must surface BOTH resulting events.
    Direct regression pin for the `changes_since` two-query race: if the
    delta-fetch and the max-seq-mint queries aren't snapshot-consistent,
    one of these two rows can be silently and permanently skipped."""
    _, xml = _sync_collection(dav_client, str(fresh_calendar.url), None)
    token = _extract_sync_token(xml)

    uid_a = f"sync-race-a-{uuid.uuid4().hex[:8]}"
    uid_b = f"sync-race-b-{uuid.uuid4().hex[:8]}"

    with ThreadPoolExecutor(max_workers=2) as pool:
        fut_a = pool.submit(_put_ical, fresh_calendar, uid_a)
        fut_b = pool.submit(_put_ical, fresh_calendar, uid_b)
        fut_a.result()
        fut_b.result()

    status, xml2 = _sync_collection(dav_client, str(fresh_calendar.url), token)
    assert status == 207
    assert uid_a in xml2, (
        f"Concurrently-written event {uid_a} missing from delta — "
        f"snapshot race regression:\n{xml2}"
    )
    assert uid_b in xml2, (
        f"Concurrently-written event {uid_b} missing from delta — "
        f"snapshot race regression:\n{xml2}"
    )


# ─────────────────────────────────────────────────────────────
# Retention / row-cap / expiry
# ─────────────────────────────────────────────────────────────


def test_calendar_sync_expired_token_after_row_cap_returns_507_then_recovers(
    dav_client: caldav.DAVClient,
    fresh_calendar: caldav.Calendar,
    base_url: str,
    admin_jwt: str,
) -> None:
    """The test server runs with a tiny
    `OXICLOUD_SYNC_LOG_MAX_ROWS_PER_COLLECTION` (see run-pycaldav.sh) so
    this is reachable without waiting out real time-based retention.

    1. Capture a token, then generate enough churn to exceed the cap.
    2. Trigger the retention job off-schedule via the admin API (the
       entry point `SyncLogRetentionService`'s job-registry migration
       added specifically to make this testable).
    3. The pre-churn token must now be rejected with 507 (RFC 6578
       §3.6) — not silently return a wrong/partial delta.
    4. A fresh full resync (empty token) afterward must succeed and
       match the server's real current state — the client's documented
       recovery path, not a dead end.
    """
    seed_uid = f"sync-cap-seed-{uuid.uuid4().hex[:8]}"
    _put_ical(fresh_calendar, seed_uid)
    _, xml = _sync_collection(dav_client, str(fresh_calendar.url), None)
    stale_token = _extract_sync_token(xml)

    # Exceed the configured cap (5) by a comfortable margin — 10
    # create+delete pairs is 20 change-log rows on this one collection.
    for i in range(10):
        uid = f"sync-cap-churn-{i}-{uuid.uuid4().hex[:6]}"
        _put_ical(fresh_calendar, uid)
        _delete_ical(fresh_calendar, uid)

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
    # Response shape: {"ok": true, "outcome": {"outcome": "ok"/"err", ...}}
    # — the outer envelope is the admin-trigger endpoint's; the inner
    # `outcome` field is the tagged `JobOutcome` (serde `tag = "outcome"`).
    outcome = trigger_resp.json()
    assert outcome.get("outcome", {}).get("outcome") == "ok", (
        f"Retention job did not report success: {outcome}"
    )

    status, body = _sync_collection(dav_client, str(fresh_calendar.url), stale_token)
    assert status == 507, (
        f"Stale token after row-cap enforcement expected HTTP 507; got "
        f"{status}\n{body}"
    )

    # Recovery path: a fresh full resync must work and reflect reality.
    status, xml2 = _sync_collection(dav_client, str(fresh_calendar.url), None)
    assert status == 207, f"Recovery full resync → HTTP {status}\n{xml2}"
    assert seed_uid in xml2, (
        f"Seed event {seed_uid} (never deleted) missing from recovery "
        f"full resync:\n{xml2}"
    )
    ground_truth = {
        _uid_from_event_data(e.data) for e in fresh_calendar.events()
    }
    recovered = set()
    _apply_delta_to_mirror(recovered, xml2, ".ics")
    assert recovered == ground_truth, (
        f"Recovery full resync doesn't match ground truth.\n"
        f"Only in resync: {recovered - ground_truth}\n"
        f"Only on server: {ground_truth - recovered}"
    )
