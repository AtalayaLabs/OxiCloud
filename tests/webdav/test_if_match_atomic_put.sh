#!/usr/bin/env bash
# Conditional PUT must compare the pre-body snapshot in the same transaction
# that replaces the file row. Otherwise concurrent requests with one ETag all
# pass the early check and silently overwrite one another.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"
source test.env
source common.sh
source lib/dav_helpers.sh

oxicloud_login
mint_app_password

FIXTURE_DIR=$(mktemp -d)

INITIAL="$FIXTURE_DIR/initial.bin"
dd if=/dev/zero of="$INITIAL" bs=4096 count=1 2>/dev/null
suffix="if-match-cas-$$"
NC_FILE_URL="$base_url/remote.php/dav/files/$username/$suffix-nc.bin"
NATIVE_FILE_URL="$base_url/webdav/$suffix-native.bin"

cleanup() {
    nc_curl -o /dev/null -X DELETE "$NC_FILE_URL" >/dev/null 2>&1 || true
    dav_curl -o /dev/null -X DELETE "$NATIVE_FILE_URL" >/dev/null 2>&1 || true
    rm -rf "$FIXTURE_DIR"
}
trap cleanup EXIT

etag_from_headers() {
    awk 'BEGIN { IGNORECASE=1 } tolower($1) == "etag:" { sub("\r$", "", $2); print $2; exit }'
}

run_race() {
    local surface="$1" url="$2" name="$3"
    local headers etag i status_file
    local writers=16
    local pids=()

    if [[ "$surface" == "nextcloud" ]]; then
        headers=$(nc_curl -D - -o /dev/null -X PUT -H "Content-Type: application/octet-stream" \
            --data-binary "@$INITIAL" "$url")
        [[ "$(awk 'NR==1 {print $2}' <<<"$headers" | tr -d '\r')" == 201 ]] \
            || fail "$surface setup PUT did not create $name"
        headers=$(nc_curl -D - -o /dev/null -I "$url")
    else
        headers=$(dav_curl -D - -o /dev/null -X PUT -H "Content-Type: application/octet-stream" \
            --data-binary "@$INITIAL" "$url")
        [[ "$(awk 'NR==1 {print $2}' <<<"$headers" | tr -d '\r')" == 201 ]] \
            || fail "$surface setup PUT did not create $name"
        headers=$(dav_curl -D - -o /dev/null -I "$url")
    fi
    etag=$(etag_from_headers <<<"$headers")
    [[ -n "$etag" ]] || fail "$surface did not return an ETag for $name"

    for ((i = 0; i < writers; i++)); do
        local payload="$FIXTURE_DIR/$surface-writer-$i.bin"
        status_file="$FIXTURE_DIR/$surface-status-$i"
        dd if=/dev/zero of="$payload" bs=262144 count=1 2>/dev/null
        printf 'writer-%02d' "$i" | dd of="$payload" conv=notrunc 2>/dev/null

        if [[ "$surface" == "nextcloud" ]]; then
            (
                nc_curl -sS -o /dev/null -w '%{http_code}' -X PUT \
                    -H "If-Match: $etag" -H "Content-Type: application/octet-stream" \
                    --data-binary "@$payload" "$url" >"$status_file" || echo 000 >"$status_file"
            ) &
        else
            (
                dav_curl -sS -o /dev/null -w '%{http_code}' -X PUT \
                    -H "If-Match: $etag" -H "Content-Type: application/octet-stream" \
                    --data-binary "@$payload" "$url" >"$status_file" || echo 000 >"$status_file"
            ) &
        fi
        pids+=("$!")
    done

    for pid in "${pids[@]}"; do
        wait "$pid" || fail "$surface concurrent PUT process failed"
    done

    local successes=0 rejected=0 unexpected=()
    for ((i = 0; i < writers; i++)); do
        status=$(<"$FIXTURE_DIR/$surface-status-$i")
        case "$status" in
            204) successes=$((successes + 1)) ;;
            412) rejected=$((rejected + 1)) ;;
            *) unexpected+=("$status") ;;
        esac
    done
    (( ${#unexpected[@]} == 0 )) \
        || fail "$surface expected only 204/412, got ${unexpected[*]}"
    (( successes == 1 && rejected == writers - 1 )) \
        || fail "$surface expected 1 winning PUT and $((writers - 1)) precondition failures; got $successes/$rejected"

    pass "$surface If-Match race: one 204, $rejected concurrent requests returned 412"
}

run_race nextcloud \
    "$NC_FILE_URL" \
    "$suffix-nc.bin"
run_race native "$NATIVE_FILE_URL" "$suffix-native.bin"
