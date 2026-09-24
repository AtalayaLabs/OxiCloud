---
title: Compare OxiCloud vs Google Drive, Dropbox, OneDrive
description: Feature-by-feature comparison of OxiCloud's current implementation against Google Drive / Workspace, Dropbox / Business, and OneDrive / Microsoft 365 — where OxiCloud is competitive, where it deliberately differs, and where features are intentionally out of scope.
---

# OxiCloud feature coverage vs Google Drive, Dropbox, OneDrive

This document compares OxiCloud's **current implementation** against the equivalent
features in Google Drive / Workspace, Dropbox / Business, and OneDrive / Microsoft 365.

It is intended as a scoping reference — it identifies where OxiCloud is
competitive, where it deliberately differs, and where features are intentionally
out of scope.

_Last update: 2026-09-25_

## Headline

OxiCloud matches the **core authorization primitives** of all three majors (user +
group + link sharing, nested groups, expiring grants, audit trail, external
invitees) plus **real-time collaborative editing** for text / markdown / code,
with several structural advantages they don't have — **open protocol interop**
(WebDAV / CardDAV / CalDAV), **self-hostable** binary, **content-addressable dedup**,
**cryptographic session binding** (OPAQUE + DPoP), and a **unified ReBAC** across
files, contacts, calendars, and drives.

It lacks the **enterprise compliance surface** (DLP, eDiscovery, sensitivity
labels, watermarking) and **multi-tenant organizational structure** (OUs,
conditional access, domain trust) that distinguishes Google Workspace /
Microsoft 365 / Dropbox Business from their consumer tiers.

Honest framing: **OxiCloud is competitive with consumer-tier Google Drive /
Dropbox / OneDrive, and partially competitive with their entry-level Business
tiers.** It does not try to be Workspace Enterprise.

## Feature matrix

Legend: ✅ shipped · 🟡 partial · ❌ not implemented · 🔮 future-friendly (door kept open)

| Capability | OxiCloud | Google Drive / Workspace | Dropbox / Business | OneDrive / M365 |
|---|---|---|---|---|
| **Authentication** | | | | |
| Password login | ✅ | ✅ | ✅ | ✅ |
| OPAQUE PAKE (RFC 9807) — server never sees the plaintext password | ✅ | ❌ | ❌ | ❌ |
| DPoP session binding (RFC 9449) — non-extractable browser P-256 key, stolen cookies alone are useless | ✅ | ❌ | ❌ | ❌ |
| OIDC SSO (as client) | ✅ + back-channel logout + RP-initiated logout propagation | ✅ (Workspace + Cloud Identity) | ✅ (Business) | ✅ (Entra ID native) |
| Self-issued OIDC (BYO IdP) — invitee brings their own IdP, OxiCloud accepts | 🔮 (design-aligned, not implemented) | ❌ | ❌ | ❌ |
| Email-challenge invitee login (magic link) | ✅ | ✅ | ✅ | ✅ (Entra B2B) |
| 2FA / MFA enforcement | ❌ (deliberate — delegate to OIDC IdP instead of shipping a second MFA stack) | ✅ | ✅ | ✅ |
| App passwords | ✅ | 🟡 (legacy) | ❌ | ✅ |
| **Sharing primitives** | | | | |
| Share with user directly | ✅ | ✅ | ✅ | ✅ |
| Share with group (named) | ✅ (subject groups + CardDAV contact groups) | ✅ | ✅ | ✅ |
| Nested groups | ✅ (depth ≤ 8, cycle-checked) | ❌ (flat) | ❌ (flat) | ✅ (AAD nesting) |
| Public link | ✅ (token-based) | ✅ | ✅ | ✅ |
| Link with password | ✅ | ❌ | ✅ (Business) | ✅ (Business) |
| Link with expiry | ✅ | ✅ | ✅ (Business) | ✅ (Business) |
| Sharing notifications (email + in-app bell) | ✅ | ✅ | ✅ | ✅ |
| Disable download / view-only | ❌ | ✅ | ✅ (Business) | ✅ (Business) |
| Watermarks | ❌ | ✅ (Workspace) | ✅ (Business+) | ✅ (E5) |
| Domain-restricted share | ✅ (operator-controlled via env — allow / block domains at the invitee-email layer) | ✅ | ✅ | ✅ |
| Expiring per-user grants | ✅ (`expires_at` on grant) | ✅ (Workspace) | ❌ | ✅ |
| **Authorization model** | | | | |
| ReBAC (subject → role → resource, with cascades) | ✅ | 🟡 (Drive ACLs cascade but no first-class ReBAC) | 🟡 | 🟡 |
| Per-resource grant (RBAC) | ✅ | ✅ | ✅ | ✅ |
| Folder → file inheritance | ✅ (`ltree @>` ancestor lookup at check time, GiST-indexed) | ✅ | ✅ | ✅ (SharePoint) |
| Group-membership transitive expansion | ✅ | ✅ | 🟡 | ✅ |
| Multiple permission types (read / write / share / comment / delete / update) | ✅ | ✅ | 🟡 (3 levels) | ✅ |
| Anti-enumeration wire shape (no-access vs no-such-resource collapse) | ✅ | ❌ | ❌ | ❌ |
| Permission audit (who & why) | ✅ | ✅ (Workspace) | ✅ (Business) | ✅ |
| **Subjects / identity** | | | | |
| Internal user | ✅ | ✅ | ✅ | ✅ |
| External invitee (no internal account) | ✅ (email-challenge or BYO OIDC) | ✅ (Google account required) | ✅ | ✅ (Entra B2B guest) |
| Anonymous public | ✅ (public link) | ✅ | ✅ | ✅ |
| Service / API token | ✅ | ✅ | ✅ | ✅ |
| Virtual "All internal" subject | ✅ (`Internal` group) | ✅ | ✅ | ✅ |
| **Group system specifics** | | | | |
| Globally unique group name | ✅ (RFC 5321 local-part) | 🟡 | ❌ | 🟡 |
| Group-as-mailing-list | 🔮 (naming preserves the door) | ✅ | ❌ | ✅ |
| Groups as resources (delegated admin) | ✅ (Manage, UseAsSubject) | 🟡 | 🟡 | ✅ |
| Group depth limit | ✅ (8) | n/a (flat) | n/a (flat) | implicit |
| **Real-time & collaboration** | | | | |
| Real-time co-editing — text / markdown / code | ✅ (CodeMirror + Yjs CRDT, peer cursors, awareness) | 🟡 (Docs plain text only) | ❌ | 🟡 (Loop, Fluid Framework) |
| Real-time co-editing — office documents | 🟡 (via WOPI bridge to Collabora / OnlyOffice) | ✅ | 🟡 (Paper) | ✅ |
| Live folder updates (see peers' uploads instantly) | ✅ | ✅ | ✅ | ✅ |
| Presence indicators / peer cursors | ✅ (Yjs awareness) | ✅ | 🟡 (Paper) | ✅ |
| Pub/sub message bus over WebSocket | ✅ (typed topics, AsyncAPI spec) | opaque | opaque | opaque |
| Comments / suggestions | ❌ | ✅ | ✅ (Paper) | ✅ |
| **Search** | | | | |
| Metadata search (name, type, date, size, folder) | ✅ | ✅ | ✅ | ✅ |
| Full-text search across file contents | ✅ (Tantivy-backed index with blob-cached extraction) | ✅ | ✅ | ✅ (SharePoint / M365) |
| OCR search over images | ❌ | ✅ | ❌ | ✅ |
| **Storage** | | | | |
| Content-addressable blob dedup (whole-file) | ✅ (BLAKE3) | ❌ (opaque) | ❌ | ❌ |
| Sub-file chunked dedup (CDC) | ✅ (FastCDC + BLAKE3 per chunk) | ❌ | ❌ | ❌ |
| Delta-upload / rsync-style transfers | ✅ (negotiate → chunks → commit) | ❌ | 🟡 (block-level for some clients) | ❌ |
| Range Requests (RFC 7233) for resumable download | ✅ | ✅ | ✅ | ✅ |
| Encryption at rest | ✅ (AES-GCM, key rotation with per-blob `key_fp`) | ✅ (opaque) | ✅ (opaque) | ✅ (opaque) |
| End-to-end encryption | ❌ (`Vault` drive kind reserved) | ❌ | ❌ | ❌ (Personal Vault ≠ E2E) |
| Multi-backend (local / S3 / Azure) | ✅ | n/a | n/a | n/a |
| Online migration between backends | ✅ (`swappable_blob_backend` + verify) | n/a | n/a | n/a |
| External mounts (S3 / WebDAV appear as folders) | ✅ | ❌ | ❌ | ❌ |
| Consistency-check jobs (discovery-only, opt-in repair) | ✅ | opaque | opaque | opaque |
| Content-aware compression (Brotli / gzip skip already-compressed) | ✅ | opaque | opaque | opaque |
| **File preview & viewers** | | | | |
| Image viewer | ✅ | ✅ | ✅ | ✅ |
| PDF viewer | ✅ | ✅ | ✅ | ✅ |
| Code viewer with syntax highlighting | ✅ (~25 languages) | 🟡 | ❌ | 🟡 |
| Video playback (with poster + thumbnails) | ✅ | ✅ | ✅ | ✅ |
| Photo gallery — timeline / places / faces | ✅ (virtualized timeline, maplibre map, face grouping) | ✅ (Google Photos) | 🟡 | ✅ (via Photos app) |
| **Operations / governance** | | | | |
| Structured audit log (stable event / reason vocabulary) | ✅ (`target: "audit"` tracing → syslog / stdout) | ✅ (Workspace Audit) | ✅ (Business) | ✅ (Purview) |
| Trash / retention | ✅ | ✅ (30d) | ✅ | ✅ |
| Versioning | ❌ (`collab.doc_snapshots` reserved) | ✅ | ✅ | ✅ |
| Recoverable background jobs (pause + resume on transient errors) | ✅ | opaque | opaque | opaque |
| Rate limiting (per-endpoint tiered) | ✅ | ✅ | ✅ | ✅ |
| DLP / sensitivity labels | ❌ | ✅ (Workspace+) | ✅ (Business+) | ✅ (Purview) |
| eDiscovery / legal hold | ❌ | ✅ (Vault) | ✅ (Business) | ✅ (Purview) |
| Conditional Access (IP / device) | ❌ | ✅ (Workspace+) | 🟡 (Business+) | ✅ (Entra CA) |
| CLI for low-level ops (rekey / migrate / OPAQUE reset) | ✅ (`oxicloud` subcommands) | n/a | n/a | n/a |
| **Multi-tenant / org** | | | | |
| Single OxiCloud instance per org | ✅ | n/a | n/a | n/a |
| Hosting many tenants in one binary | ❌ | ✅ | ✅ | ✅ |
| Organizational Units / sub-admin | ❌ | ✅ | 🟡 (Teams) | ✅ |
| Domain trust between orgs | ❌ | ✅ | 🟡 | ✅ |
| **Client / protocol reach** | | | | |
| WebDAV native | ✅ | 🟡 (export only) | ❌ | 🟡 |
| CardDAV native | ✅ | 🟡 | ❌ | 🟡 |
| CalDAV native | ✅ | 🟡 | ❌ | 🟡 |
| Nextcloud-compatible REST API | ✅ | ❌ | ❌ | ❌ |
| WOPI office bridge | ✅ | ✅ | 🟡 | ✅ |
| OpenAPI + AsyncAPI specs published | ✅ (drift-gated in CI) | 🟡 | 🟡 | 🟡 |
| Open Cloud Mesh (OCM) — federated sharing between different cloud instances | 🔮 (design-aligned; not implemented yet) | ❌ | ❌ | ❌ |
| Self-hostable open source | ✅ | ❌ | ❌ | ❌ |

## Where OxiCloud differentiates (positively)

1. **Self-hostable + open protocols.** No public-cloud competitor offers this.
   For privacy-sensitive workloads (legal, medical, research, government) this
   is the headline value.
2. **Cryptographic session binding via DPoP.** Stealing a session cookie from
   another host is worthless without the browser-held P-256 key. None of the
   majors offer this to end-users — session security relies on cookie
   confidentiality, which loses to info-stealers on compromised endpoints.
3. **OPAQUE password login.** The server never sees the plaintext password.
   A DB dump or wire capture yields nothing replayable elsewhere. Nobody at
   consumer / small-team scale offers this today.
4. **Real-time collaborative editing built on Yjs CRDT** for text / markdown /
   code. Live peer cursors, capabilities-gated read-only, eviction on
   grant-revoke or external write. First-class in the platform — not delegated
   to an office app.
5. **Content-addressable storage with BLAKE3 dedup — both whole-file and
   sub-file (CDC).** A storage-efficiency advantage no proprietary cloud
   surfaces to admins. Uploads short-circuit on hash match; large files with
   small edits re-transmit only the touched chunks.
6. **Unified ReBAC across every domain object.** One coherent authorization
   model spans files, folders, drives, contacts, calendars, address books,
   music, and collab sessions. The majors have separate ACL systems per
   product line; OxiCloud unifies them under a single grant model.
7. **Folder-tree inheritance done as a single SQL operator** (`ltree @>`)
   against a GiST index. One grant row covers an entire subtree, evaluated
   at check time in O(log N).
8. **Native nested groups with cycle protection and depth cap.** Better than
   Dropbox (no nesting) and Google Groups (flat).
9. **External mounts** — S3 / WebDAV / etc. providers appear as folders in
   the user's tree. Users see their existing cloud storage alongside
   OxiCloud-native storage without leaving the app.
10. **Anti-enumeration by construction.** Every AuthZ denial and every unknown
    resource returns the same wire shape. No side-channel for guessing which
    files exist.
11. **AsyncAPI + OpenAPI both published and drift-gated in CI.** Every wire
    change requires regenerated specs to merge — no drift between "what the
    docs say" and "what the server does".

## Where the design is honestly missing things

Listed in rough priority of how often customers would ask:

1. **Versioning** — file history / restore previous versions. Big gap vs all
   three majors. `collab.doc_snapshots` reserves the schema; the user-facing
   history + restore surface is deferred.
2. **Sensitivity labels / DLP** — enterprise-only, but customers in regulated
   industries will ask.
3. **Comments / annotations on files** — Drive / Dropbox / OneDrive all have
   this; OxiCloud doesn't.
4. **Disable download / view-only restrictions** — a small but visible
   feature gap.
5. **2FA enforcement policies for password-only users** — MFA for OIDC users
   is expected to come from the IdP (that's the preferred stance). What's
   missing today is an admin knob to require MFA for users who log in with
   OxiCloud's local password (no IdP in the loop). Note: the
   password-authenticated part of the trust chain already uses OPAQUE +
   DPoP, so replay attacks are much harder than in a plain-cookie stack —
   but a second factor is still stronger than one.
6. **End-to-end encryption** — the `Vault` drive kind is reserved for this
   but the client-held-key crypto isn't wired.
7. **Multi-tenancy** — needed for a hosting solution; deliberately deferred.
8. **Conditional Access (IP / device)** — enterprise table-stakes; out of
   scope for now.
9. **eDiscovery / legal hold** — heavily regulated industries only.
10. **User-initiated self-service password recovery** — admin-initiated reset
    works today; the "forgot my password" magic-link flow for end-users has
    a session-elevation gap noted in the plan.

## Where the design intentionally does it differently

- **Group naming as RFC 5321 local-part.** Google does this (group emails)
  but doesn't enforce uniqueness for sharing; the OxiCloud approach is more
  principled.
- **External users live in the same `auth.users` table with a flag,** not in
  a parallel B2B-guests table (Microsoft's choice). Simpler reasoning, no
  dual-identity migration on upgrade.
- **CardDAV contact groups stay separate from ACL subject groups.** Microsoft
  has historically conflated security groups and distribution lists;
  OxiCloud's explicit split (content vs policy) is cleaner.
- **No "Everyone including external" virtual subject.** Only `Internal` is
  predefined. External users are reached explicitly per-grant, not via a
  catch-all. This avoids accidental over-sharing.
- **Audit to structured tracing (syslog / stdout / any subscriber)** rather
  than a proprietary "Audit Log" UI. Open-source friendly, integrates with
  existing SIEM tooling. Every event carries a stable `event` +
  machine-readable `reason` field so aggregators can key off them without
  breaking on release-note wording changes.
- **Open authorization data model** — grants are just rows in the schema.
  Customers can write their own admin queries against the DB; no magical
  black box.
- **Consistency checks are discovery-only by default.** Repair is opt-in
  (`?repair=true`). Silent auto-heal is refused as a category — the operator
  decides whether to touch anything based on what the check found.
- **Recoverable jobs pause at a cursor on transient failure** rather than
  either retry forever or falsely report success. Nothing silently drops
  work.
- **Decentralised by intent — federation over centralisation.** Many
  instances federating beats one large instance. Open Cloud Mesh (OCM) is
  the target protocol; the design deliberately avoids assumptions that
  would only hold on a single authoritative deployment. Wire, storage, and
  identity choices all leave room for a user on instance A to share with a
  user on instance B without either instance having to know about the other
  ahead of time.

## Scope summary

OxiCloud is a **complete consumer-and-small-team cloud** (matches Google Drive
personal, Dropbox basic, OneDrive consumer) with **uniquely strong properties
on open protocols, self-hosting, cryptographic session hygiene, and
collaborative editing**.

Gaps that would block adoption in regulated enterprise sectors — versioning,
DLP, eDiscovery, conditional access, multi-tenancy — are deliberate
omissions for now; each is a multi-month design effort of its own.
