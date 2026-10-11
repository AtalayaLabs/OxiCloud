"""Keep DAV response URLs in the collection alias the client synchronized.

Issue #751's DAVx5 trace requests /caldav/admin/<id>/ but receives members
under /caldav/<id>/. A client comparing resource URLs can interpret that
change as removal of the uploaded object. Check actual response hrefs after
a PUT on both DAV surfaces, including the streamed and buffered emitters.
"""

from __future__ import annotations

import uuid
import xml.etree.ElementTree as ET
from urllib.parse import quote, urlsplit
from xml.sax.saxutils import escape

import pytest

from test_carddav import _minimal_vcard
from test_report import _dedent


@pytest.fixture(params=["caldav", "carddav"])
def dav_collection(request, caldav_username):
    protocol = request.param
    if protocol == "caldav":
        original = str(request.getfixturevalue("fresh_calendar").url)
    else:
        original = request.getfixturevalue("fresh_addressbook")
    collection_id = urlsplit(original).path.rstrip("/").split("/")[-1]
    root = request.getfixturevalue(f"{protocol}_url").rstrip("/")
    return protocol, root, collection_id, caldav_username


@pytest.mark.parametrize("user_home", [False, True])
@pytest.mark.parametrize(
    "operation",
    ["collection-propfind", "collection-propfind-zero", "member-propfind", "query", "range-query", "multiget", "sync"],
)
def test_response_hrefs_preserve_the_uploaded_collection(
    dav_client, dav_collection, user_home, operation
):
    protocol, root, collection_id, username = dav_collection
    if protocol == "carddav" and operation == "range-query":
        pytest.skip("CardDAV has no calendar time-range query")
    prefix = f"{quote(username, safe='')}/" if user_home else ""
    collection_url = f"{root}/{prefix}{collection_id}/"
    uid = f"href-{uuid.uuid4().hex}"
    suffix = ".ics" if protocol == "caldav" else ".vcf"
    member_url = f"{collection_url}{uid}{suffix}"
    if protocol == "caldav":
        content = _dedent(f"""
            BEGIN:VCALENDAR
            VERSION:2.0
            PRODID:-//OxiCloud href regression//EN
            BEGIN:VEVENT
            UID:{uid}
            DTSTAMP:20260101T080000Z
            DTSTART:20260101T090000Z
            DTEND:20260101T100000Z
            SUMMARY:Collection href regression
            END:VEVENT
            END:VCALENDAR
        """)
        content_type = "text/calendar"
    else:
        content = _minimal_vcard(uid)
        content_type = "text/vcard"
    result = dav_client.request(
        member_url, method="PUT", body=content,
        headers={"Content-Type": content_type},
    )
    assert result.status == 201, result.raw

    collection_path = urlsplit(collection_url).path
    member_path = urlsplit(member_url).path
    props = "<D:prop><D:getetag/></D:prop>"
    target = collection_url
    expected = {member_path}
    method = "REPORT"
    depth = "1"
    if "propfind" in operation:
        method = "PROPFIND"
        body = f'<D:propfind xmlns:D="DAV:">{props}</D:propfind>'
        if operation == "member-propfind":
            target = member_url
        elif operation == "collection-propfind-zero":
            depth = "0"
            expected = {collection_path}
        else:
            expected.add(collection_path)
    elif operation == "sync":
        body = (
            '<D:sync-collection xmlns:D="DAV:">'
            f'<D:sync-token/><D:sync-level>1</D:sync-level>{props}'
            '</D:sync-collection>'
        )
    else:
        kind = "calendar" if protocol == "caldav" else "addressbook"
        namespace = "urn:ietf:params:xml:ns:" + protocol
        report = f"{kind}-multiget" if operation == "multiget" else f"{kind}-query"
        if operation == "multiget":
            query = f"<D:href>{escape(member_path)}</D:href>"
        elif protocol == "caldav":
            time_range = (
                '<C:time-range start="20260101T000000Z" end="20260102T000000Z"/>'
                if operation == "range-query" else ""
            )
            query = (
                '<C:filter><C:comp-filter name="VCALENDAR">'
                f'<C:comp-filter name="VEVENT">{time_range}</C:comp-filter>'
                '</C:comp-filter></C:filter>'
            )
        else:
            query = "<C:filter/>"
        body = f'<C:{report} xmlns:D="DAV:" xmlns:C="{namespace}">{props}{query}</C:{report}>'

    result = dav_client.request(
        target, method=method, body=body,
        headers={"Content-Type": "application/xml", "Depth": depth},
    )
    assert result.status == 207, result.raw
    document = ET.fromstring(result.raw)
    hrefs = {node.text for node in document.findall("{DAV:}response/{DAV:}href")}
    assert hrefs == expected, (
        f"{protocol} {operation} changed resource URLs after PUT to {member_url}: {hrefs}"
    )
    # A listing must not merely advertise the right URL: it must still resolve
    # to the uploaded resource when the client follows it.
    fetched = dav_client.request(member_url, method="GET")
    assert fetched.status == 200, fetched.raw
    text = fetched.raw.decode() if isinstance(fetched.raw, bytes) else fetched.raw
    assert f"UID:{uid}" in text
