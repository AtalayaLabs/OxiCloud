"""Shared pytest fixtures for the pycaldav conformance suite.

Environment (injected by `run-pycaldav.sh`):
  OXICLOUD_CALDAV_URL          — base CalDAV URL, e.g. http://localhost:8091/caldav/
  OXICLOUD_CALDAV_USERNAME     — admin username
  OXICLOUD_CALDAV_APP_PASSWORD — app password (NOT the account password)

The suite deliberately talks to the same URL a real CalDAV client
would — via HTTP Basic + an app password, no JWT. That's how
Thunderbird, Apple Calendar, DAVx⁵ and Gnome Calendar all connect.
"""

from __future__ import annotations

import logging
import os
import re
import uuid

import caldav
import pytest


# ─────────────────────────────────────────────────────────────
# Silence pycaldav's chatty logging during test setup.
#
# python-caldav's `make_calendar()` internally does MKCALENDAR +
# PROPPATCH-displayname. OxiCloud's MKCALENDAR assigns its own
# server-side UUID (spec deviation, see fresh_calendar fixture),
# so the follow-up PROPPATCH lands on a URL the server doesn't
# know → 500 / 404. pycaldav catches and moves on ("calendar
# server does not support display name on calendar? Ignoring"),
# but its handler logs at CRITICAL with `exc_info=True`, dumping
# a full XMLSyntaxError traceback under pytest's "Captured log
# setup" section on every test. That noise dwarfed real
# assertion output.
#
# Filtering at logger level here has nothing to capture, so the
# traceback disappears from the pytest output.
# ─────────────────────────────────────────────────────────────
logging.getLogger("caldav").setLevel(logging.ERROR)
logging.getLogger("caldav.davclient").setLevel(logging.ERROR)
# pycaldav uses `logging.critical(..., exc_info=True)` on the ROOT
# logger for the "expected XML, got JSON" case. `setLevel(ERROR)`
# does NOT hide CRITICAL (CRITICAL > ERROR), so use the override
# switch instead: `logging.disable(CRITICAL)` disables every level
# up to and INCLUDING CRITICAL, killing pycaldav's setup traceback
# spam outright. run-pycaldav.sh also passes `--show-capture=no`
# so any remaining captured output is hidden on failure — defence
# in depth, since one clean-output knob is easier to forget than two.
logging.getLogger().setLevel(logging.ERROR)
logging.disable(logging.CRITICAL)


def _env(name: str) -> str:
    value = os.environ.get(name)
    if not value:
        raise RuntimeError(
            f"Missing required env var {name}. Run this suite via "
            "tests/caldav/run-pycaldav.sh (or `just test-caldav`) which "
            "bootstraps admin + app password before invoking pytest."
        )
    return value


@pytest.fixture(scope="session")
def caldav_url() -> str:
    return _env("OXICLOUD_CALDAV_URL")


@pytest.fixture(scope="session")
def caldav_username() -> str:
    return _env("OXICLOUD_CALDAV_USERNAME")


@pytest.fixture(scope="session")
def caldav_app_password() -> str:
    return _env("OXICLOUD_CALDAV_APP_PASSWORD")


@pytest.fixture(scope="session")
def admin_jwt() -> str:
    """Bearer JWT for the admin account, minted by `run-pycaldav.sh`.

    Needed for admin-only endpoints (`/api/admin/...`) — app passwords /
    DAV Basic Auth don't satisfy the `require_admin` middleware, only a
    JWT with the admin role does."""
    return _env("OXICLOUD_ADMIN_JWT")


@pytest.fixture(scope="session")
def base_url(caldav_url: str) -> str:
    """Server origin, derived from `caldav_url` — same derivation style
    as `carddav_url` below. Needed for endpoints outside the
    `/caldav/`-rooted `dav_client` (e.g. `/api/admin/...`)."""
    if "/caldav/" not in caldav_url:
        raise RuntimeError(
            f"OXICLOUD_CALDAV_URL={caldav_url!r} does not contain "
            "'/caldav/'; can't derive the base origin."
        )
    return caldav_url.split("/caldav/", 1)[0]


@pytest.fixture(scope="session")
def dav_client(
    caldav_url: str, caldav_username: str, caldav_app_password: str
) -> caldav.DAVClient:
    """The single DAVClient used across the session — python-caldav
    reuses one requests.Session under the hood."""
    return caldav.DAVClient(
        url=caldav_url,
        username=caldav_username,
        password=caldav_app_password,
    )


@pytest.fixture
def fresh_calendar(dav_client: caldav.DAVClient):
    """A brand-new calendar per test. The name is randomised so parallel
    workers (`pytest -n auto` in the future) don't collide, and every
    test teardown drops the calendar — no cross-test bleed.

    Server-URL rebind: OxiCloud's MKCALENDAR assigns its own UUID and
    ignores the URL slug the client PUT to (design choice — the URL
    slug becomes the display name when the request body is empty; the
    canonical URL is `/caldav/<server-uuid>/`). python-caldav's
    `make_calendar()` returns a Calendar bound to the client-derived
    URL, which then 404s on every subsequent op. Re-discover the
    server-authoritative URL by listing the principal's calendars and
    matching by displayname."""
    principal = dav_client.principal()
    name = f"pycaldav-{uuid.uuid4().hex[:12]}"
    principal.make_calendar(name=name)

    calendar = next(
        (c for c in principal.calendars() if c.get_display_name() == name),
        None,
    )
    if calendar is None:
        raise RuntimeError(
            f"MKCALENDAR completed but the new calendar '{name}' did not "
            "appear in principal.calendars() — server-side provisioning "
            "issue."
        )

    yield calendar
    try:
        calendar.delete()
    except Exception:
        # Teardown is best-effort — if a test crashed the server, we
        # don't want the teardown crash to mask the real failure.
        pass


# ─────────────────────────────────────────────────────────────
# CardDAV fixtures — python-caldav has no first-class CardDAV
# support, so these drive the server via raw HTTP through the
# same authenticated DAVClient session. Kept in this conftest
# (not a sibling tests/carddav/ dir) for now — one venv, one
# `just test-caldav` entry point. If the CardDAV coverage
# grows past ~one file's worth, promote to tests/carddav/ with
# its own runner.
# ─────────────────────────────────────────────────────────────


@pytest.fixture(scope="session")
def carddav_url(caldav_url: str) -> str:
    """CardDAV base URL derived from the CalDAV URL — the
    orchestrator only exports `OXICLOUD_CALDAV_URL`, but the
    server mounts both under the same origin. Swap `/caldav/`
    for `/carddav/`."""
    if "/caldav/" not in caldav_url:
        raise RuntimeError(
            f"OXICLOUD_CALDAV_URL={caldav_url!r} does not contain "
            "'/caldav/'; can't derive the CardDAV counterpart."
        )
    return caldav_url.replace("/caldav/", "/carddav/", 1)


@pytest.fixture
def fresh_addressbook(dav_client: caldav.DAVClient, carddav_url: str):
    """Create a fresh CardDAV address book and return its
    server-authoritative URL as a string.

    Same URL-rebind hazard as `fresh_calendar`: OxiCloud's MKCOL
    assigns its own UUID and ignores the URL slug we PUT to
    (RFC 6352 leaves this implementation-defined). Discover the
    canonical URL via PROPFIND Depth 1 on the CardDAV root and
    match by displayname.

    Yields the URL (string, trailing `/`); teardown DELETEs it
    on best-effort."""
    name = f"pycarddav-{uuid.uuid4().hex[:12]}"

    mkcol_url = carddav_url.rstrip("/") + f"/{name}/"
    r = dav_client.request(mkcol_url, method="MKCOL", body="")
    if r.status not in (200, 201):
        raise RuntimeError(
            f"MKCOL {mkcol_url} → HTTP {r.status}\n{r.raw!r}"
        )

    propfind_body = (
        '<?xml version="1.0" encoding="UTF-8"?>'
        '<D:propfind xmlns:D="DAV:">'
        "<D:prop><D:displayname/><D:resourcetype/></D:prop>"
        "</D:propfind>"
    )
    r = dav_client.request(
        carddav_url,
        method="PROPFIND",
        body=propfind_body,
        headers={"Depth": "1", "Content-Type": "application/xml"},
    )
    if r.status < 200 or r.status >= 300:
        raise RuntimeError(
            f"PROPFIND {carddav_url} → HTTP {r.status}\n{r.raw!r}"
        )
    xml = r.raw.decode("utf-8") if isinstance(r.raw, bytes) else r.raw

    # Naive but sufficient: iterate <D:response> blocks; pick the
    # one whose block text contains our chosen displayname; pull
    # its <D:href> as the canonical URL slug.
    href = None
    for block in re.finditer(
        r"<D:response>(.*?)</D:response>", xml, flags=re.DOTALL
    ):
        chunk = block.group(1)
        if name in chunk:
            m = re.search(r"<D:href>(/carddav/[^<]+/)</D:href>", chunk)
            if m:
                href = m.group(1)
                break
    if href is None:
        raise RuntimeError(
            f"MKCOL succeeded but PROPFIND did not surface an address "
            f"book with displayname '{name}':\n{xml}"
        )

    # href from the server is a path (e.g. `/carddav/<uuid>/`);
    # combine with the URL origin to get an absolute URL usable in
    # subsequent `dav_client.request()` calls.
    origin = re.match(r"^(https?://[^/]+)", carddav_url).group(1)
    ab_url = f"{origin}{href}"

    yield ab_url
    try:
        dav_client.request(ab_url, method="DELETE")
    except Exception:
        pass
