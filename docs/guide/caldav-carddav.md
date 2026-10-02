# CalDAV & CardDAV

OxiCloud provides built-in CalDAV (calendar) and CardDAV (contacts) servers — no extra apps or plugins needed.

## Authentication

CalDAV and CardDAV clients authenticate with an **app password**, not
your regular OxiCloud account password. Your account password is
refused on `/caldav/` and `/carddav/` (same as `/webdav/`). This is by
design — app passwords are the only credential shape that works
uniformly across all account types (password, magic-link-only, OIDC).

**Generate one:** in OxiCloud web UI, go to **Profile → App Passwords**,
click *Create*, name it (e.g. "Thunderbird calendar"), and copy the
token shown once. Use your username + that token in every DAV client
below.

See [DAV Client Setup](./dav-client-setup#before-you-start-get-an-app-password)
for full details.

## CalDAV (Calendars)

### Endpoint

```
https://your-server:8086/caldav/
```

### Protocol Compliance

- RFC 4791 (Calendar Access)
- RFC 5545 (iCalendar format)
- DAV capabilities: `1, 2, calendar-access`

### Route Structure

CalDAV is mounted at the top level, not under `/api`:

- `/caldav`
- `/caldav/`
- `/caldav/{*path}`

OxiCloud also exposes `/.well-known/caldav` and redirects it to `/caldav/`.

Typical resource shapes:

- `/caldav/` for the calendar home
- `/caldav/{calendar_id}/` for one calendar
- `/caldav/{calendar_id}/{ical_uid}.ics` for one event

### Supported Methods

- `OPTIONS`
- `PROPFIND`
- `REPORT`
- `MKCALENDAR`
- `PUT`
- `GET`
- `DELETE`
- `PROPPATCH`

### Shared Calendars

A calendar shared through `POST /api/grants` (`resource.type = "calendar"`)
appears in the recipient's calendar home. What their CalDAV client may do
follows the role it was shared with, and is advertised through
`current-user-privilege-set` so clients mount it read-only or read-write
accordingly:

| Role | Advertised privileges | Allowed over CalDAV |
|------|-----------------------|---------------------|
| Viewer, Commenter | `read` | Read events and tasks |
| Contributor | `read`, `bind` | …and add new ones (a `PUT` over an existing UID is refused) |
| Editor | `read`, `write-content`, `bind`, `unbind` | …and modify or delete any event or task |
| Owner | `all` | …and rename / recolour the calendar (`PROPPATCH`) or delete it |

A refused write returns `403 Forbidden`.

### Client Setup

| Client | URL |
|--------|-----|
| Thunderbird | `https://your-server:8086/caldav/` |
| GNOME Calendar | `https://your-server:8086/caldav/` |
| Apple Calendar (macOS/iOS) | `https://your-server:8086/caldav/` |
| DAVx⁵ (Android) | `https://your-server:8086/` (auto-discovery) |

### Thunderbird Setup

1. Open Thunderbird → **Calendar** tab
2. Right-click → **New Calendar** → **On the Network**
3. Format: **CalDAV**
4. URL: `https://your-server:8086/caldav/`
5. Enter your OxiCloud username and an [app password](#authentication) — the account password is refused

---

## CardDAV (Contacts)

### Endpoint

```
https://your-server:8086/carddav/
```

### Protocol Compliance

- RFC 6352 (CardDAV)
- RFC 6350 (vCard 4.0)

### Route Structure

CardDAV is also mounted at the top level:

- `/carddav`
- `/carddav/`
- `/carddav/{*path}`

Typical resource shapes:

- `/carddav/` for the address book home
- `/carddav/{addressBookId}/` for one address book
- `/carddav/{addressBookId}/{contactId}.vcf` for one contact

### Supported Methods

- `OPTIONS`
- `PROPFIND`
- `REPORT`
- `MKCOL`
- `PUT`
- `GET`
- `DELETE`
- `PROPPATCH`

### Client Setup

| Client | URL |
|--------|-----|
| Thunderbird | `https://your-server:8086/carddav/` |
| GNOME Contacts | `https://your-server:8086/carddav/` |
| Apple Contacts (macOS/iOS) | `https://your-server:8086/carddav/` |
| DAVx⁵ (Android) | `https://your-server:8086/` (auto-discovery) |

### DAVx⁵ (Android) Setup

1. Install [DAVx⁵](https://www.davx5.com/) from F-Droid or Play Store
2. Add account → **Login with URL and user name**
3. Base URL: `https://your-server:8086/`
4. Enter your OxiCloud username and an [app password](#authentication) — the account password is refused
5. DAVx⁵ auto-discovers both CalDAV and CardDAV endpoints

::: info
DAVx⁵ file sync works. CalDAV/CardDAV support on DAVx⁵ is still being refined.
:::

## Client Setup

For platform-specific instructions, see [DAV Client Setup](/guide/dav-client-setup).
