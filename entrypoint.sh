#!/bin/sh
set -e

# Fix ownership of mounted volumes.
# When Docker creates named volumes they are owned by root, but the
# application runs as the unprivileged "oxicloud" user. This script
# runs as root, (optionally) retargets the oxicloud user's UID/GID,
# fixes permissions, then drops privileges.

STORAGE_DIR="/app/storage"
STATIC_DIR="/app/static"

# Operator-overridable UID/GID the oxicloud user runs as.
#
# Defaults match the Dockerfile's `addgroup -g 1001 / adduser -u 1001`,
# so a zero-config `docker run` behaves exactly as before. Operators
# whose host-side storage dataset is owned by a different UID/GID
# (TrueNAS apps default to 568:568, Synology to 1026:100, etc.) set
# PUID/PGID in the env and the entrypoint rewrites the baked-in
# oxicloud user to match before the chown + su-exec. Issue #842 is
# the canonical "my dataset is owned by 568 and I can't change it"
# case.
#
# Precedence over the Docker-native `user: <uid>:<gid>` directive:
# the `user:` directive takes priority. If compose sets `user: 568:568`
# the container starts as 568 (not root), this script sees non-root,
# skips the UID rewrite + the chown, and runs the app as 568. Operator
# assumes responsibility for host-side ownership in that path.
# PUID/PGID is for the "started as root, want to drop to arbitrary
# UID" shape.
PUID="${PUID:-1001}"
PGID="${PGID:-1001}"

# Recursively chown DIR to the oxicloud user, but only when its top-level
# entry is not already owned by that user. The storage volume is a
# content-addressable blob store that can hold millions of objects; a blind
# "chown -R" on every boot would re-stat and rewrite the inode of every blob,
# turning startup into minutes of disk I/O. Checking the root entry is the
# cheap idempotent guard: the first boot fixes a freshly mounted (root-owned)
# volume, and every later boot is a no-op.
ensure_owned() {
    dir="$1"
    if [ -d "$dir" ] && [ "$(stat -c %u "$dir")" != "$OXI_UID" ]; then
        chown -R oxicloud:oxicloud "$dir"
    fi
}

# Only root can chown or rewrite passwd/group; when started
# unprivileged the volume permissions and user mapping are assumed to
# be correct already.
if [ "$(id -u)" -eq 0 ]; then
    CUR_UID="$(id -u oxicloud)"
    CUR_GID="$(id -g oxicloud)"

    # Rewrite /etc/passwd + /etc/group directly rather than pulling the
    # `shadow` package (~2 MB) for `usermod` / `groupmod`. The sed
    # anchors on `^oxicloud:x:<cur>:` so only the oxicloud line is
    # touched; unrelated system users (root, nobody, …) stay as-is.
    # Idempotent: if PUID/PGID already match, both sed calls are
    # no-ops (regex matches but substitution produces the same bytes).
    if [ "$PGID" != "$CUR_GID" ]; then
        sed -i "s/^oxicloud:x:${CUR_GID}:/oxicloud:x:${PGID}:/" /etc/group
    fi
    if [ "$PUID" != "$CUR_UID" ] || [ "$PGID" != "$CUR_GID" ]; then
        # Second column of the passwd gid field also needs updating if
        # the group id changed, hence substitute both fields together.
        sed -i "s/^oxicloud:x:${CUR_UID}:${CUR_GID}:/oxicloud:x:${PUID}:${PGID}:/" /etc/passwd
    fi

    OXI_UID="$(id -u oxicloud)"
    ensure_owned "$STORAGE_DIR"
    ensure_owned "$STATIC_DIR"
fi

# Drop privileges and exec the main binary (or whatever was passed as CMD)
if [ "$(id -u)" -eq 0 ]; then
    exec su-exec oxicloud "$@"
else
    "$@"
fi
