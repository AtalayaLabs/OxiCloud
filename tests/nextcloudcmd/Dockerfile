# Nextcloud desktop sync engine, command-line front end (`nextcloudcmd`).
#
# There is no official image, so this one is built from Debian's package and
# pinned three ways: base image by digest, apt archive by snapshot.debian.org
# timestamp, and the package by exact version. Bumping the client is a
# deliberate edit of CLIENT_VERSION + SNAPSHOT, never a silent drift.
#
# Licence: nextcloud-desktop is GPL-2.0-or-later. It is used here as an
# external test tool in a container that is built locally and never
# published; nothing from it is linked into or shipped with OxiCloud (MIT).

FROM debian:trixie-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad

ARG SNAPSHOT=20261001T000000Z
ARG CLIENT_VERSION=3.16.7-1~deb13u1

RUN rm -f /etc/apt/sources.list.d/debian.sources \
 && echo "deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/${SNAPSHOT} trixie main" \
      > /etc/apt/sources.list \
 && apt-get update -q \
 && apt-get install -y -q --no-install-recommends "nextcloud-desktop-cmd=${CLIENT_VERSION}" \
 && rm -rf /var/lib/apt/lists/*

# Qt warns on every start under the default C locale.
ENV LANG=C.UTF-8

ENTRYPOINT ["nextcloudcmd"]
