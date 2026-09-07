#!/usr/bin/env bash
set -euo pipefail

COMPOSE_FILE="$(dirname "$0")/docker-compose.test.yml"

wait_for_port() {
  local host="$1" port="$2" timeout="${3:-30}"
  local deadline=$(( $(date +%s) + timeout ))
  until nc -z "$host" "$port" 2>/dev/null; do
    [[ $(date +%s) -ge $deadline ]] && echo "Timeout waiting for $host:$port" >&2 && exit 1
    sleep 0.5
  done
}

# Postgres opens its TCP socket during `initdb` (well before the server can
# actually serve queries) and slams it back shut until startup completes —
# that's the classic "server closed the connection unexpectedly" race when
# a follow-up `psql` runs too quickly. The compose file has a `pg_isready`
# healthcheck; we mirror it here so this script is the single source of
# "the DB is ready" for callers (tests/api/run.sh, just test-integration).
#
# Tries the in-container pg_isready first (always available, no host deps),
# then falls back to a SELECT-1 probe via psql.
wait_for_postgres_ready() {
  local timeout="${1:-60}"
  local deadline=$(( $(date +%s) + timeout ))
  until docker compose -f "$COMPOSE_FILE" exec -T postgres-test \
          pg_isready -U oxicloud_test -d oxicloud_test -h 127.0.0.1 >/dev/null 2>&1; do
    [[ $(date +%s) -ge $deadline ]] && echo "Timeout waiting for postgres readiness" >&2 && exit 1
    sleep 0.5
  done
  # Belt-and-braces: pg_isready returns 0 as soon as the server accepts
  # connections, but a query may still race the very first request. One
  # successful SELECT confirms the round-trip works end-to-end.
  #
  # Retry the probe a handful of times — under CPU pressure (e.g. a parallel
  # cargo build hammering a self-hosted runner) the role/db init can complete
  # a beat after pg_isready returns success. A single-shot probe in that
  # window produces spurious "Postgres reported ready but a sample query
  # failed" failures. Show the last error if every retry fails so operators
  # see the actual psql diagnostic.
  local last_err
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    if last_err=$(PGPASSWORD=oxicloud_test psql -h 127.0.0.1 -p 5433 \
                    -U oxicloud_test -d oxicloud_test \
                    -v ON_ERROR_STOP=1 -c 'SELECT 1' 2>&1 >/dev/null); then
      return 0
    fi
    sleep 0.5
  done
  echo "Postgres reported ready but a sample query failed after 10 retries: $last_err" >&2
  exit 1
}

# Create the blob container Azurite serves.
#
# `AzureBlobBackend::initialize` VERIFIES the container exists and fails
# with a 404 if it does not — it deliberately does not create one, since
# auto-creating would turn a typo'd container name into a silently
# working empty container. So the harness provisions it, exactly as an
# operator would in production.
#
# Signed by hand rather than shelling out to the Azure CLI: `az` would
# mean pulling a ~700 MB image into every CI run to issue one PUT. This
# needs only curl and openssl, both already required.
#
# Two things make this fiddly enough to be worth commenting:
#   * The key is base64 and HMAC needs raw bytes, so it is decoded and
#     re-encoded as hex for `-macopt hexkey:`.
#   * The canonicalized resource repeats the account name —
#     `/{account}/{account}/{container}` — because the emulator puts the
#     account in the URL path where real Azure puts it in the host. This
#     is the classic Azurite signing trap; getting it wrong yields 403,
#     not a hint.
#
# Idempotent by result: a second run gets 409 ContainerAlreadyExists,
# which is success for our purposes.
create_azurite_container() {
  local acc=devstoreaccount1
  local cont=oxicloud-test
  # Azurite's fixed development key — hardcoded in the image, published
  # by Microsoft, and valid against nothing but a local emulator.
  local key='Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=='
  local ver=2021-08-06
  local date_hdr hexkey sts sig code
  date_hdr=$(LC_ALL=C TZ=GMT date '+%a, %d %b %Y %H:%M:%S GMT')
  hexkey=$(printf '%s' "$key" | base64 -d | od -An -tx1 | tr -d ' \n')
  # Twelve leading empty lines are the unused standard headers
  # (Content-*, Date, If-*, Range) the SharedKey scheme requires in
  # fixed positions.
  sts="PUT\n\n\n\n\n\n\n\n\n\n\n\nx-ms-date:${date_hdr}\nx-ms-version:${ver}\n/${acc}/${acc}/${cont}\nrestype:container"
  sig=$(printf '%b' "$sts" | openssl dgst -sha256 -mac HMAC -macopt "hexkey:${hexkey}" -binary | base64)
  code=$(curl -s -o /dev/null -w '%{http_code}' -X PUT \
    -H "x-ms-date: ${date_hdr}" \
    -H "x-ms-version: ${ver}" \
    -H "Authorization: SharedKey ${acc}:${sig}" \
    "http://127.0.0.1:10000/${acc}/${cont}?restype=container" 2>/dev/null)
  case "$code" in
    201) echo "[setup] Azurite container '${cont}' created." ;;
    409) echo "[setup] Azurite container '${cont}' already exists." ;;
    *)   echo "[setup] WARNING: could not create Azurite container '${cont}' (HTTP ${code}) — Azure-backed tests will fail." >&2 ;;
  esac
}

echo "[setup] Starting test postgres + azurite..."
docker compose -f "$COMPOSE_FILE" down -v 2>/dev/null || true
docker compose -f "$COMPOSE_FILE" up -d
echo "[setup] Waiting for postgres on port 5433..."
wait_for_port 127.0.0.1 5433
echo "[setup] Waiting for postgres to accept queries..."
wait_for_postgres_ready
echo "[setup] Postgres is ready."

# Azurite backs the `azurite` storage entry, which `backend_consistency`
# audits via `?storage=azurite`. Only a port wait: the entry is declared
# but never activated, so nothing in the suite touches it until a test
# names it, and by then the listener has had the whole server boot to
# settle. A failure here should not take down a run that mostly does not
# use it — so this warns rather than exits, and the Azure test fails on
# its own terms with a clearer message than "setup timed out".
#
# Its own loop rather than `wait_for_port`: that helper calls `exit 1` on
# timeout, and `exit` inside a function ends the script whatever context
# it was called from — so wrapping it in an `if` would not degrade, it
# would just fail later and less clearly.
echo "[setup] Waiting for azurite on port 10000..."
azurite_deadline=$(( $(date +%s) + 30 ))
until nc -z 127.0.0.1 10000 2>/dev/null; do
  if [[ $(date +%s) -ge $azurite_deadline ]]; then
    echo "[setup] WARNING: azurite did not come up — Azure-backed tests will fail." >&2
    break
  fi
  sleep 0.5
done
if nc -z 127.0.0.1 10000 2>/dev/null; then
  # Port open only means the listener is up — NOT that the tests can use
  # it. Saying "ready" here was misleading: the container was still
  # missing, so the first Azure job failed with a 404 while setup had
  # already reported success. `create_azurite_container` is what makes
  # the claim true, so it is the one that gets to announce it.
  echo "[setup] Azurite listening; provisioning container..."
  create_azurite_container
fi
