"""VTODO tasks (#754) and TZID / VTIMEZONE (#689) conformance via python-caldav.

Two feature surfaces, both over the wire shape a real client uses:

  * VTODO — AtalayaLabs/OxiCloud#754. Tasks clients (DAVx⁵ +
    Tasks.org / jtx Board, Thunderbird, Apple Reminders) sync VTODO
    components over the same collections as events. The server stores
    them in `caldav.calendar_todos` and serves the stored iCalendar
    blob verbatim, so EVERY property (PRIORITY, STATUS,
    PERCENT-COMPLETE, CATEGORIES, X-*) round-trips byte-exact.
  * TZID — AtalayaLabs/OxiCloud#689. `DTSTART;TZID=Europe/Paris:...`
    must be ACCEPTED (pre-fix: HTTP 400) and converted through the
    IANA tz database so the indexed columns hold the correct UTC
    instant; VTIMEZONE blocks (RRULEs and all) are preserved and
    served back byte-exact — DST evaluation stays client-side.

Row-level HTTP assertions use the same `_put_ical` / raw-GET pattern
as test_report.py; client-model assertions go through python-caldav's
`calendar.todos()` / `calendar.search(todo=True)`.
"""

from __future__ import annotations

import textwrap
import uuid
from datetime import datetime, timezone

import caldav


# ─────────────────────────────────────────────────────────────
# Helpers — mirror the pattern from test_report.py. Deliberately
# duplicated for now; promote to conftest.py once a fourth test
# file shows up.
# ─────────────────────────────────────────────────────────────


def _dedent(ical: str) -> str:
    return textwrap.dedent(ical).strip().replace("\n", "\r\n") + "\r\n"


def _put_ical(calendar: caldav.Calendar, uid: str, body: str) -> None:
    url = str(calendar.url).rstrip("/") + f"/{uid}.ics"
    r = calendar.client.request(
        url,
        method="PUT",
        body=body,
        headers={"Content-Type": "text/calendar; charset=utf-8"},
    )
    if r.status < 200 or r.status >= 300:
        raise AssertionError(
            f"PUT {url} → HTTP {r.status}\nbody: {body!r}\nresponse: {r.raw!r}"
        )


def _get_raw(calendar: caldav.Calendar, uid: str) -> str:
    """GET the .ics object and return the raw body text — the only
    way to assert byte-level round-trip (VTIMEZONE blocks, folded
    lines, X-properties) without a model layer in between."""
    url = str(calendar.url).rstrip("/") + f"/{uid}.ics"
    r = calendar.client.request(url, method="GET")
    if r.status != 200:
        raise AssertionError(f"GET {url} → HTTP {r.status}")
    return str(r.raw)


# ─────────────────────────────────────────────────────────────
# VTODO (#754)
# ─────────────────────────────────────────────────────────────


def test_vtodo_round_trip_preserves_every_property(
    fresh_calendar: caldav.Calendar,
) -> None:
    """PUT a kitchen-sink VTODO, GET it back raw: every property —
    indexed (PRIORITY, STATUS, DUE) or not (CATEGORIES, X-CUSTOM) —
    must come back byte-exact. This is the 'blob authoritative,
    columns index-only' contract."""
    uid = f"todo-{uuid.uuid4().hex[:8]}"
    _put_ical(
        fresh_calendar,
        uid,
        _dedent(
            f"""\
            BEGIN:VCALENDAR
            VERSION:2.0
            PRODID:-//pycaldav vtodo coverage//EN
            BEGIN:VTODO
            UID:{uid}
            DTSTAMP:20260919T100000Z
            SUMMARY:Buy milk
            DESCRIPTION:2% or oat
            STATUS:IN-PROCESS
            PERCENT-COMPLETE:40
            PRIORITY:1
            DTSTART:20260920T090000Z
            DUE:20260925T180000Z
            CATEGORIES:ERRANDS,HOME
            X-CUSTOM-FLAG:keep-me
            END:VTODO
            END:VCALENDAR
            """
        ),
    )

    raw = _get_raw(fresh_calendar, uid)
    for needle in [
        "BEGIN:VTODO",
        "SUMMARY:Buy milk",
        "DESCRIPTION:2% or oat",
        "STATUS:IN-PROCESS",
        "PERCENT-COMPLETE:40",
        "PRIORITY:1",
        "DUE:20260925T180000Z",
        "CATEGORIES:ERRANDS,HOME",
        "X-CUSTOM-FLAG:keep-me",
    ]:
        assert needle in raw, f"{needle!r} lost in VTODO round-trip. Got:\n{raw}"

    # And the client model finds it through the todos() surface.
    todos = fresh_calendar.todos()
    assert any(uid in (t.data or "") for t in todos), (
        f"Seeded todo {uid} invisible to calendar.todos() — "
        "depth-1 listing / comp-filter VTODO query broken?"
    )


def test_vtodo_minimal_body_accepted(fresh_calendar: caldav.Calendar) -> None:
    """RFC 5545 §3.6.2 mandates only UID + DTSTAMP on a VTODO. A
    minimal task (no SUMMARY, no DTSTART, no DUE) must not 400."""
    uid = f"todo-min-{uuid.uuid4().hex[:8]}"
    _put_ical(
        fresh_calendar,
        uid,
        _dedent(
            f"""\
            BEGIN:VCALENDAR
            VERSION:2.0
            PRODID:-//pycaldav vtodo coverage//EN
            BEGIN:VTODO
            UID:{uid}
            DTSTAMP:20260919T100000Z
            END:VTODO
            END:VCALENDAR
            """
        ),
    )
    raw = _get_raw(fresh_calendar, uid)
    assert uid in raw


def test_vtodo_time_range_search_uses_due_window(
    fresh_calendar: caldav.Calendar,
) -> None:
    """RFC 4791 §9.9 VTODO overlap: a task whose DUE falls inside
    the window matches; the same task must NOT match a disjoint
    window."""
    uid = f"todo-due-{uuid.uuid4().hex[:8]}"
    _put_ical(
        fresh_calendar,
        uid,
        _dedent(
            f"""\
            BEGIN:VCALENDAR
            VERSION:2.0
            PRODID:-//pycaldav vtodo coverage//EN
            BEGIN:VTODO
            UID:{uid}
            DTSTAMP:20260919T100000Z
            SUMMARY:File taxes
            DUE:20260925T180000Z
            END:VTODO
            END:VCALENDAR
            """
        ),
    )

    hits = fresh_calendar.search(
        start=datetime(2026, 9, 25, 0, 0, tzinfo=timezone.utc),
        end=datetime(2026, 9, 26, 0, 0, tzinfo=timezone.utc),
        todo=True,
        expand=False,
    )
    assert any(uid in (t.data or "") for t in hits), (
        f"Task with DUE inside the window missing from todo search"
    )

    misses = fresh_calendar.search(
        start=datetime(2026, 10, 1, 0, 0, tzinfo=timezone.utc),
        end=datetime(2026, 10, 2, 0, 0, tzinfo=timezone.utc),
        todo=True,
        expand=False,
    )
    assert not any(uid in (t.data or "") for t in misses), (
        f"Task leaked into a disjoint window — DUE-based filter broken"
    )


def test_comp_filter_routing_event_search_excludes_todos(
    fresh_calendar: caldav.Calendar,
) -> None:
    """comp-filter VEVENT must not return tasks and vice versa —
    the two kinds share the collection but route to separate
    stores (#754)."""
    event_uid = f"evt-{uuid.uuid4().hex[:8]}"
    todo_uid = f"todo-{uuid.uuid4().hex[:8]}"
    _put_ical(
        fresh_calendar,
        event_uid,
        _dedent(
            f"""\
            BEGIN:VCALENDAR
            VERSION:2.0
            BEGIN:VEVENT
            UID:{event_uid}
            DTSTAMP:20260919T100000Z
            DTSTART:20260921T100000Z
            DTEND:20260921T110000Z
            SUMMARY:Not a task
            END:VEVENT
            END:VCALENDAR
            """
        ),
    )
    _put_ical(
        fresh_calendar,
        todo_uid,
        _dedent(
            f"""\
            BEGIN:VCALENDAR
            VERSION:2.0
            BEGIN:VTODO
            UID:{todo_uid}
            DTSTAMP:20260919T100000Z
            SUMMARY:Not an event
            DUE:20260921T120000Z
            END:VTODO
            END:VCALENDAR
            """
        ),
    )

    event_hits = fresh_calendar.search(
        start=datetime(2026, 9, 21, 0, 0, tzinfo=timezone.utc),
        end=datetime(2026, 9, 22, 0, 0, tzinfo=timezone.utc),
        event=True,
        expand=False,
    )
    assert any(event_uid in (e.data or "") for e in event_hits)
    assert not any(todo_uid in (e.data or "") for e in event_hits), (
        "VTODO leaked into a comp-filter VEVENT search"
    )


# ─────────────────────────────────────────────────────────────
# TZID + VTIMEZONE (#689)
# ─────────────────────────────────────────────────────────────


def test_tzid_event_accepted_and_indexed_at_correct_utc_instant(
    fresh_calendar: caldav.Calendar,
) -> None:
    """The exact #689 reproducer: `DTSTART;TZID=Europe/Paris` must
    be ACCEPTED (was HTTP 400) and the event must land at the
    correct UTC instant — Paris in August is CEST = UTC+2, so
    12:00 local is 10:00 UTC."""
    uid = f"tz-{uuid.uuid4().hex[:8]}"
    _put_ical(
        fresh_calendar,
        uid,
        _dedent(
            f"""\
            BEGIN:VCALENDAR
            VERSION:2.0
            PRODID:-//TZID test//EN
            BEGIN:VEVENT
            UID:{uid}
            DTSTAMP:20260824T094500Z
            DTSTART;TZID=Europe/Paris:20260824T120000
            DTEND;TZID=Europe/Paris:20260824T130000
            SUMMARY:TZID test
            END:VEVENT
            END:VCALENDAR
            """
        ),
    )

    # The TZID form round-trips verbatim.
    raw = _get_raw(fresh_calendar, uid)
    assert "DTSTART;TZID=Europe/Paris:20260824T120000" in raw

    # And the server-side index converted to the right UTC instant:
    # a search over the 10:00–11:00 UTC window finds the event…
    hits = fresh_calendar.search(
        start=datetime(2026, 8, 24, 10, 0, tzinfo=timezone.utc),
        end=datetime(2026, 8, 24, 11, 0, tzinfo=timezone.utc),
        event=True,
        expand=False,
    )
    assert any(uid in (e.data or "") for e in hits), (
        "TZID event not indexed at the correct UTC instant (10:00Z)"
    )

    # …while the wall-clock-as-UTC window (12:00–13:00Z) does NOT —
    # that's where a server that ignores TZID would have placed it.
    misses = fresh_calendar.search(
        start=datetime(2026, 8, 24, 12, 0, tzinfo=timezone.utc),
        end=datetime(2026, 8, 24, 13, 0, tzinfo=timezone.utc),
        event=True,
        expand=False,
    )
    assert not any(uid in (e.data or "") for e in misses), (
        "TZID event indexed as wall-clock-as-UTC — TZID ignored?"
    )


def test_vtimezone_block_round_trips_with_rrules(
    fresh_calendar: caldav.Calendar,
) -> None:
    """A PUT body carrying a VTIMEZONE definition must come back
    with the block — TZID, both observances' RRULEs, offsets —
    byte-exact. The server stores and returns the definition; DST
    evaluation stays client-side."""
    uid = f"tz-akl-{uuid.uuid4().hex[:8]}"
    _put_ical(
        fresh_calendar,
        uid,
        _dedent(
            f"""\
            BEGIN:VCALENDAR
            VERSION:2.0
            PRODID:-//TZID test//EN
            BEGIN:VTIMEZONE
            TZID:Pacific/Auckland
            BEGIN:DAYLIGHT
            TZNAME:NZDT
            TZOFFSETFROM:+1200
            TZOFFSETTO:+1300
            DTSTART:19700927T020000
            RRULE:FREQ=YEARLY;BYMONTH=9;BYDAY=-1SU
            END:DAYLIGHT
            BEGIN:STANDARD
            TZNAME:NZST
            TZOFFSETFROM:+1300
            TZOFFSETTO:+1200
            DTSTART:19700405T030000
            RRULE:FREQ=YEARLY;BYMONTH=4;BYDAY=1SU
            END:STANDARD
            END:VTIMEZONE
            BEGIN:VEVENT
            UID:{uid}
            DTSTAMP:20260101T100000Z
            DTSTART;TZID=Pacific/Auckland:20260115T120000
            DTEND;TZID=Pacific/Auckland:20260115T130000
            SUMMARY:Auckland meeting
            END:VEVENT
            END:VCALENDAR
            """
        ),
    )

    raw = _get_raw(fresh_calendar, uid)
    for needle in [
        "BEGIN:VTIMEZONE",
        "TZID:Pacific/Auckland",
        "BEGIN:DAYLIGHT",
        "BEGIN:STANDARD",
        "RRULE:FREQ=YEARLY;BYMONTH=9;BYDAY=-1SU",
        "RRULE:FREQ=YEARLY;BYMONTH=4;BYDAY=1SU",
        "TZOFFSETTO:+1300",
        "TZOFFSETFROM:+1200",
        "DTSTART;TZID=Pacific/Auckland:20260115T120000",
    ]:
        assert needle in raw, f"{needle!r} lost in VTIMEZONE round-trip. Got:\n{raw}"

    # Auckland in January is NZDT = UTC+13 → 12:00 local on
    # 2026-01-15 is 23:00 UTC on 2026-01-14.
    hits = fresh_calendar.search(
        start=datetime(2026, 1, 14, 23, 0, tzinfo=timezone.utc),
        end=datetime(2026, 1, 15, 0, 0, tzinfo=timezone.utc),
        event=True,
        expand=False,
    )
    assert any(uid in (e.data or "") for e in hits), (
        "Auckland-anchored event not indexed at 23:00Z the day before"
    )
