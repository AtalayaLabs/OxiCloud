# Drives

A **drive** is a self-contained storage space with its own folder tree,
its own members, its own quota, and its own settings. Drives are how
OxiCloud separates "my personal files" from "our team's shared files"
without mixing them together.

Every user who signs up with a full account gets a **Personal drive**
right away. On top of that, an admin can create **Shared drives** for
teams, projects, or departments.

> **Guest recipients (from a shared link)** don't get a Personal
> drive. They only see the specific items that have been shared with
> them.

## Personal drives vs Shared drives

| | Personal drive | Shared drive |
|---|---|---|
| **Who owns it** | You | A person, or a group of people |
| **Who can see the content** | Only you (until you share individual items) | Every member |
| **How storage is counted** | Against your personal quota | Against the drive's own quota |
| **Can members be added** | No — it's yours alone | Yes — that's the point |
| **Who creates it** | Created for you at sign-up | Created by your admin |
| **Can it be deleted** | No — it lives as long as your account does | Yes, by the drive Owner |

You can't turn your Personal drive into a Shared drive, or the other
way round — they're different things by design.

## Switching between drives

Your drives appear in the sidebar under **Drives**. Click one and the
file browser jumps into that drive's contents. The breadcrumb at the
top of the file view always shows which drive you're currently in, so
you never wonder where a file will land when you upload it.

If someone else has added you to their Shared drive, it shows up in
the sidebar automatically — nothing to accept or install.

## Roles inside a Shared drive

Inside a Shared drive, each member has a **role** that decides what
they can do. Each role includes everything the role above it allows.

| Role | What they can do |
|---|---|
| **Viewer** | Browse the drive and open files. See the trash bin (but not act on it). |
| **Editor** | Plus upload new files, create folders, rename, and modify existing content. |
| **Owner** | Plus delete files, share files with people outside the drive, rename the drive, add and remove members. |

Your Personal drive doesn't have members or roles — it's always just
you, and you can do everything.

> **Editors can't delete.** In a Shared drive, only Owners send files
> to the trash or empty it. If an Editor uploads a file by mistake,
> they ask an Owner to remove it. This is deliberate — it prevents an
> Editor from clearing content the team relies on. Personal drives
> don't have this restriction (you're always your own Owner).

## Drive settings — what an Owner can change

Drive Owners can rename the drive and manage its members: add someone,
remove someone (except the last remaining Owner), change a member's
role, or set an expiration date on a membership.

Other settings — **quota** and **policies** (see below) — are set by
your OxiCloud admin, not by drive Owners. This is a compliance
choice: if Owners could relax a policy, mint a share, and then
re-enable the policy, the policy wouldn't really enforce anything.
Owners can see the current settings, but changing them goes through
the admin.

## Policies — per-drive guardrails

Every drive comes with a set of **policies** — safety switches that
shape what's allowed inside the drive. Only admins can turn them on
or off. Members see the current setting when it affects what they
can do.

| Policy | What it controls |
|---|---|
| **Sharing individual files** | Whether members can share specific files or folders with people outside the drive. When off, access happens only through drive membership. |
| **Public links** | Whether members can create anonymous "anyone with the link" URLs. Turn off for anything sensitive. |
| **Inviting people by email** | Whether members can share with someone who doesn't have an account yet (an email invitation with a magic-link sign-in). |
| **Cross-drive move** | Whether files can be moved from this drive into another drive. Turn off to prevent members from relocating content out of a sensitive drive via the UI. |
| **Owner list changes** | Locks the Owner roster. After the admin sets the Owners, no Owner can add, remove, or demote another Owner — only the admin can. |
| **Include in Photos** | Whether photos in this drive appear in the global **Photos** view. Off by default for non-default drives; turn on for shared drives that really are photo libraries (e.g. "Family Photos"). |
| **Include in Music** | Whether audio files in this drive appear in the global **Music** view. Same shape as photos — off by default, on for drives that are actually music libraries. |
| **Read-only (freeze)** | Full freeze. When on, **every mutation on the drive is refused** — new files, edits, deletes, renames, sharing, membership changes. Members can still read and download. Nothing on the drive changes until the admin unfreezes it. Use for archives, publications, legal holds, or account wind-downs. |

> **Cross-drive move blocks the UI move, not download-then-re-upload.**
> If you need to stop content from ever leaving a drive, you need
> stricter controls (file-egress policies are a future feature).

> **Read-only is a hard freeze.** Even the trash-retention janitor
> pauses on a read-only drive — items past their normal 30-day
> lifetime stay in trash until the drive is unfrozen. This is
> intentional: the whole point of the freeze is that *nothing*
> changes, including automated cleanup. Once unfrozen, the next
> retention pass catches up on anything that aged during the freeze.

## Storage and quota

- **Personal drive files** count against your account's storage
  quota. If you're near your limit, uploads to your Personal drive
  stop working until you free space.
- **Shared drive files** count against the drive's own quota, set by
  the admin. Your account quota isn't affected by files in Shared
  drives — collaborating in a 1 TB Shared drive costs you no personal
  bytes.
- Two identical files stored in different drives are only stored once
  on disk. Deduplication happens behind the scenes, so a file shared
  between drives doesn't cost double.

## Trash — one per drive

Every drive has its own trash bin. Deleting a file in a Shared drive
moves it into that drive's trash — not into your Personal drive's
trash. This keeps each drive's history self-contained.

- **Viewers** see the trash so they know what's been removed.
- **Owners** restore items or empty the trash.
- Deleting a whole Shared drive also empties its trash — nothing
  spills into other drives.

See [Trash & Recycle Bin](/guide/trash) for the standard trash
lifetime and behaviour.

## Sharing individual files and folders

Sharing an individual file or folder works exactly the same in any
drive — see [Sharing](/guide/sharing) for the full recipe. The
drive's **policies** (above) might restrict some options (no public
links, no email invitations) — the share dialog just hides the
options that are disallowed.

The drive itself isn't public-linkable. If an outsider needs to see
one file from a Shared drive, share **that file** with them — not the
whole drive.

## Photos, Music, Favorites, Recent, Search — how drives affect them

| Feature | What you see |
|---|---|
| **Photos** | Photos from your Personal drive, plus any Shared drive whose admin turned on **Include in Photos**. |
| **Music** | Same as Photos — Personal drive plus opted-in Shared drives. |
| **Playlists** | Your playlists can pull tracks from any drive you have access to (they're a curation tool, not tied to a specific drive). |
| **Favorites** | Anything you've starred, across every drive you can reach. |
| **Recent** | Files you've touched recently, across every drive you can reach. |
| **Search** | Searches every drive you have access to. |

Losing access to a drive removes its content from these views the
next time they load — no stale entries.

## Drives in WebDAV clients

Native WebDAV works with all your drives. When you connect a client
like **Finder**, **Cyberduck**, **Windows Explorer**, or **rclone**
to `.../webdav/`, you land in your Personal drive by default — so a
single-drive user's bookmark keeps working.

To reach Shared drives, browse to `.../webdav/@drive/`. That folder
lists every drive you have access to. Pick one and you're inside it,
just like a regular folder.

- Bookmark `.../webdav/@drive/` if you regularly switch drives —
  it's your "drive picker."
- Bookmark `.../webdav/@drive/<drive-name>/` if you usually work in
  one specific Shared drive — that's your fastest route in.
- Access-denied and "no such drive" look identical from a client
  (both return "not found") — that's deliberate, to avoid leaking
  which drives exist.

::: warning Sync clients: pick ONE drive
Never point a mirroring client (like `rclone sync`, a Finder mount,
or a scripted `curl` loop) at `.../webdav/@drive/`. It would try to
mirror **every** drive you can reach into local disk — twice for
your Personal drive contents, and once for every large Shared drive
you happen to be a member of.

For sync, always target one specific drive: either bare
`.../webdav/` (your Personal drive) or `.../webdav/@drive/<name>/`
(one Shared drive).
:::

## Drives in Nextcloud clients

Nextcloud desktop, Android, and iOS clients connect to your account
and sync your **Personal drive**. Adding the account
in the app "just works" without any extra configuration.

A Nextcloud client always syncs your Personal drive; a Nextcloud
account has no drive choice. With `OXICLOUD_LINKS_MODE=auto`, the
Shared drives you are a member of appear as folders under `Shares/`
in that Personal drive, and sync with it. Shares you receive from
others appear there too.

## Quick recipes

**See which drives I can access.**
Open the sidebar. Every drive you can reach is under **Drives**, with
your Personal drive first.

**Move a file from my Personal drive into a Shared drive.**
Open the file → *More* → *Move* → pick the target drive → pick a
folder → *Move here*. Your Personal quota goes down; the Shared
drive's quota goes up. (Only works if the target drive's **Cross-drive
move** policy allows it.)

**Get a Shared drive for a team.**
Ask an admin — Shared drive creation is an admin action. Tell them
who the Owner should be (a person or a group), and what quota you
need.

**Add someone to a Shared drive I own.**
Open the drive → *Members* → *Add* → pick a person or a group → pick
a role → *Save*.

**Change a member's role.**
Open the drive → *Members* → click the member → change the role →
*Save*.

**Set an expiration on a member.**
Open the drive → *Members* → click the member → set an **expiration
date** → *Save*. After that date they lose access automatically.

**Turn off public links or email invitations for a sensitive drive.**
Ask an admin. They can flip either policy per-drive. Existing links
stop working when the policy changes; members can't create new ones.

**Freeze a drive (legal hold, archive, wind-down).**
Ask an admin to set the **Read-only** policy on the drive. From that
moment, no member — including Owners — can add, edit, delete,
rename, share, or change membership. Reads and downloads keep
working. The trash retention janitor also pauses on the drive, so
items past their normal lifetime stay put. When the hold is over,
the admin turns Read-only off and mutation resumes exactly where it
was; retention catches up on the next tick.

**Restore something from a Shared drive's trash.**
Open the drive → *Trash* → pick the item → *Restore*. (Only Owners
of the drive can do this. Viewers and Editors can see the trash but
not act on it.)

**Access a Shared drive from Finder / Cyberduck / Windows Explorer.**
Connect to `.../webdav/@drive/`. The drives you can reach appear as
folders; pick one and go.

**Sync a Shared drive with the Nextcloud desktop app.**
With `OXICLOUD_LINKS_MODE=auto`, the Shared drive appears under
`Shares/` in your Personal drive and syncs with the account.
