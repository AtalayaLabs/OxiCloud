# Nextcloud desktop sync engine, command-line front end (`nextcloudcmd`).
#
# There is no official image, so this one is built from Debian's package and
# pinned two ways: base image by digest, and the package by exact version.
# Bumping the client is a deliberate edit of CLIENT_VERSION, never a silent
# drift: when a Debian point release replaces that version, apt-get fails with
# "Version ... was not found" and the pin has to be updated. Dependencies of the
# package do follow trixie's point releases.
#
# Not snapshot.debian.org, which pinned the archive too: it is too slow and
# drops requests under load. Each dropped request schedules a delayed apt retry,
# and apt then spun forever logging "Tried to start delayed item ..., but
# failed" until the CI job died with exit 137. Retrying without the delay only
# moved the failure: a cold build was still unfinished after 20 minutes.
#
# Licence: nextcloud-desktop is GPL-2.0-or-later. It is used here as an
# external test tool in a container that is built locally and never
# published; nothing from it is linked into or shipped with OxiCloud (MIT).

FROM debian:trixie-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad

ARG CLIENT_VERSION=3.16.7-1~deb13u1

# The timeouts make a stalled mirror fail the build loudly instead of hanging
# until the job's own limit.
RUN rm -f /etc/apt/sources.list.d/debian.sources \
 && echo "deb http://deb.debian.org/debian trixie main" > /etc/apt/sources.list \
 && timeout -k 10 300 apt-get update -q \
 && timeout -k 10 600 apt-get install -y -q --no-install-recommends "nextcloud-desktop-cmd=${CLIENT_VERSION}" \
 && rm -rf /var/lib/apt/lists/*

# Qt warns on every start under the default C locale.
ENV LANG=C.UTF-8

ENTRYPOINT ["nextcloudcmd"]
