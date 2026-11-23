#!/usr/bin/env bash
# ============================================================================
# OxiCloud Performance Benchmark v3
# Detailed RAM (RSS/VmPeak/VmSwap) + CPU monitoring + huge file downloads
#
#  - Uploads: 1KB..500MB via API (above ~1GB the upload handler OOMs —
#    that's a known issue in the multipart handler which buffers chunks).
#  - Downloads: 1KB..10GB.  For files >500MB we inject blobs directly into
#    the blob store + DB so the streaming download path is tested without
#    going through the buffering upload handler.
# ============================================================================
set -uo pipefail

BASE_URL="${BASE_URL:-http://127.0.0.1:8086}"
USERNAME="${BENCH_USER:-benchuser}"
PASSWORD="${BENCH_PASS:-BenchPass123!}"
FOLDER_ID="${BENCH_FOLDER:-0e72efc0-0d1c-45a1-b434-52336643b3f7}"
USER_ID="${BENCH_USER_ID:-c410b103-7b86-4ac2-9eb4-3804351547be}"
WORKDIR="/tmp/oxibench"
RESULTS="$WORKDIR/results_v3.csv"
STORAGE="./storage"
MONITOR_INTERVAL=0.2  # 200ms sample rate

# ── Colours ──────────────────────────────────────────────────────────────────
RED='\033[0;31m'; GRN='\033[0;32m'; YEL='\033[1;33m'
CYA='\033[0;36m'; BLD='\033[1m'; DIM='\033[2m'; NC='\033[0m'

header() { printf "\n${BLD}${CYA}═══════════════════════════════════════════${NC}\n"; \
           printf "${BLD}${CYA}  %s${NC}\n" "$1"; \
           printf "${BLD}${CYA}═══════════════════════════════════════════${NC}\n"; }
ok()     { printf "${GRN}✓${NC} %s\n" "$1"; }
warn()   { printf "${YEL}⚠${NC} %s\n" "$1"; }
fail()   { printf "${RED}✗${NC} %s\n" "$1"; }

# ── Server PID ───────────────────────────────────────────────────────────────
SERVER_PID=""
find_server_pid() {
    SERVER_PID=$(pgrep -f "target/release/oxicloud" 2>/dev/null | head -1 || \
                 pgrep -f "target/debug/oxicloud"   2>/dev/null | head -1 || echo "")
    [[ -z "$SERVER_PID" ]] && { fail "Server PID not found"; exit 1; }
}

# ── Memory (KB) ─────────────────────────────────────────────────────────────
get_rss_kb()   { awk '/^VmRSS:/  {print $2}' /proc/$SERVER_PID/status 2>/dev/null || echo 0; }
get_peak_kb()  { awk '/^VmPeak:/ {print $2}' /proc/$SERVER_PID/status 2>/dev/null || echo 0; }
get_swap_kb()  { awk '/^VmSwap:/ {print $2}' /proc/$SERVER_PID/status 2>/dev/null || echo 0; }
get_vsize_kb() { awk '/^VmSize:/ {print $2}' /proc/$SERVER_PID/status 2>/dev/null || echo 0; }

# ── CPU jiffies ──────────────────────────────────────────────────────────────
get_cpu_jiffies() { awk '{print $14+$15}' /proc/$SERVER_PID/stat 2>/dev/null || echo 0; }

# ── Background monitor ──────────────────────────────────────────────────────
MONITOR_PID=""
MONITOR_RESULT="$WORKDIR/_mon"
MON_PEAK_RSS=0; MON_PEAK_VM=0; MON_PEAK_SWAP=0; MON_CPU_PCT="0.0"

start_monitor() {
    rm -f "$MONITOR_RESULT"
    (
        peak_rss=0; peak_vm=0; peak_swap=0
        cpu0=$(get_cpu_jiffies); t0=$(date +%s%N)
        while true; do
            r=$(get_rss_kb); v=$(get_vsize_kb); s=$(get_swap_kb)
            (( r > peak_rss  )) && peak_rss=$r
            (( v > peak_vm   )) && peak_vm=$v
            (( s > peak_swap )) && peak_swap=$s
            sleep $MONITOR_INTERVAL 2>/dev/null || break
        done
        cpu1=$(get_cpu_jiffies); t1=$(date +%s%N)
        dt=$(( cpu1 - cpu0 )); wall=$(( (t1 - t0) / 1000000 ))
        cpup=0
        (( wall > 0 )) && cpup=$(echo "scale=1; $dt * 10 * 100 / $wall" | bc 2>/dev/null || echo 0)
        echo "$peak_rss $peak_vm $peak_swap $cpup" > "$MONITOR_RESULT"
    ) &
    MONITOR_PID=$!
}

stop_monitor() {
    [[ -n "$MONITOR_PID" ]] && { kill $MONITOR_PID 2>/dev/null; wait $MONITOR_PID 2>/dev/null || true; MONITOR_PID=""; }
    if [[ -f "$MONITOR_RESULT" ]]; then
        read -r MON_PEAK_RSS MON_PEAK_VM MON_PEAK_SWAP MON_CPU_PCT < "$MONITOR_RESULT"
    else
        MON_PEAK_RSS=0; MON_PEAK_VM=0; MON_PEAK_SWAP=0; MON_CPU_PCT="0.0"
    fi
}

# ── Auth ─────────────────────────────────────────────────────────────────────
TOKEN=""
login() {
    TOKEN=$(curl -sf -X POST "$BASE_URL/api/auth/login" \
      -H "Content-Type: application/json" \
      -d "{\"username\":\"$USERNAME\",\"password\":\"$PASSWORD\"}" \
      | python3 -c "import sys,json; print(json.load(sys.stdin)['access_token'])" 2>/dev/null || echo "")
    [[ -z "$TOKEN" ]] && { fail "JWT login failed"; exit 1; }
}

# ── File gen ─────────────────────────────────────────────────────────────────
gen_file() {
    local path="$1" bytes="$2"
    local bs=1048576
    local count=$(( bytes / bs )) rem=$(( bytes % bs ))
    (( count > 0 )) && dd if=/dev/urandom of="$path" bs=$bs count=$count 2>/dev/null
    (( rem   > 0 )) && dd if=/dev/urandom bs=$rem count=1 >> "$path" 2>/dev/null
}

human() {
    local b=$1
    if   (( b >= 1073741824 )); then printf "%.2f GB" "$(echo "$b/1073741824" | bc -l)"
    elif (( b >=    1048576 )); then printf "%.1f MB" "$(echo "$b/1048576"    | bc -l)"
    elif (( b >=       1024 )); then printf "%.1f KB" "$(echo "$b/1024"       | bc -l)"
    else printf "%d B" "$b"; fi
}

throughput() {
    local b=$1 ms=$2
    (( ms <= 0 )) && { echo "---"; return; }
    human $(echo "$b*1000/$ms" | bc 2>/dev/null || echo 0)
}

mb() { echo $(( $1 / 1024 )); }  # KB -> MB

# ── Inject large blob (bypass upload handler) ──────────────────────────────
# Creates a random file as a blob, registers it in DB, returns file_id.
# Usage: inject_blob <label> <size_bytes>
inject_blob() {
    local label="$1" size="$2"
    local hash fpath blob_dir blob_path file_id

    hash=$(printf "%s_%s" "$label" "$(date +%s%N)" | sha256sum | awk '{print $1}')
    blob_dir="$STORAGE/.blobs/${hash:0:2}"
    blob_path="$blob_dir/${hash}.blob"
    mkdir -p "$blob_dir"

    printf "  Generating blob %s (%s) ..." "$label" "$(human $size)" >&2
    gen_file "$blob_path" "$size"
    printf " done\n" >&2

    # Register blob in dedup index
    docker exec -i $(docker ps -qf "name=postgres") psql -U postgres -d oxicloud -q <<SQL >/dev/null 2>&1
INSERT INTO storage.blobs (hash, size, content_type, ref_count)
VALUES ('$hash', $size, 'application/octet-stream', 1)
ON CONFLICT DO NOTHING;
SQL

    # Register file metadata
    file_id=$(docker exec -i $(docker ps -qf "name=postgres") psql -U postgres -d oxicloud -t -A <<SQL 2>/dev/null
INSERT INTO storage.files (name, folder_id, user_id, blob_hash, size, mime_type)
VALUES ('bench_${label}.bin', '$FOLDER_ID'::uuid, '$USER_ID', '$hash', $size, 'application/octet-stream')
RETURNING id::text;
SQL
    )
    file_id=$(echo "$file_id" | grep -oE '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}' | head -1)
    echo "$file_id"
}

# ── Setup ────────────────────────────────────────────────────────────────────
mkdir -p "$WORKDIR"
rm -f "$RESULTS"
echo "test,size_bytes,direction,latency_ms,rss_before_mb,rss_peak_mb,rss_after_mb,swap_peak_mb,cpu_pct,throughput_MBs,http_code" > "$RESULTS"

find_server_pid

header "OxiCloud Benchmark v3 — RAM + CPU + Huge Files"
printf "  %-14s %s\n"   "Server"       "$BASE_URL"
printf "  %-14s %s\n"   "PID"          "$SERVER_PID"
printf "  %-14s %d MB\n" "RSS now"     $(mb $(get_rss_kb))
printf "  %-14s %d MB\n" "VmPeak"      $(mb $(get_peak_kb))
printf "  %-14s %d MB\n" "Swap"        $(mb $(get_swap_kb))
printf "  %-14s %s\n"   "Cores"        "$(nproc)"
printf "  %-14s %s\n"   "Disk free"    "$(df -h $WORKDIR | tail -1 | awk '{print $4}')"
login; ok "Authenticated"
echo

# ── Upload sizes (safe for the buffering handler) ───────────────────────────
UPLOAD_SIZES=(
    "1KB:1024"
    "64KB:65536"
    "256KB:262144"
    "1MB:1048576"
    "10MB:10485760"
    "50MB:52428800"
    "100MB:104857600"
    "250MB:262144000"
    "500MB:524288000"
)

# Additional sizes for download-only (injected directly as blobs)
DOWNLOAD_ONLY_SIZES=(
    "1GB:1073741824"
    "2GB:2147483648"
    "5GB:5368709120"
    "10GB:10737418240"
)

# ── Generate upload test files ──────────────────────────────────────────────
header "Generating Upload Test Files"
for e in "${UPLOAD_SIZES[@]}"; do
    l="${e%%:*}"; b="${e##*:}"
    f="$WORKDIR/test_${l}.bin"
    if [[ -f "$f" ]] && [[ $(stat -c%s "$f" 2>/dev/null || echo 0) -eq "$b" ]]; then
        printf "  %s exists\n" "$l"
    else
        printf "  %s (%s) ..." "$l" "$(human $b)"
        gen_file "$f" "$b"
        printf " done\n"
    fi
done

# ============================================================================
#  UPLOADS  (via API multipart)
# ============================================================================
header "Upload Benchmarks (via API)"
printf "${BLD}%-8s │ %8s %10s │ %7s %7s %7s %6s │ %5s │ %4s${NC}\n" \
       "Size" "Latency" "Thruput" "RSSbef" "RSSpk" "RSSaft" "Swap" "CPU%" "HTTP"
printf "%-8s │ %8s %10s │ %7s %7s %7s %6s │ %5s │ %4s\n" \
       "------" "-------" "--------" "------" "-----" "------" "----" "----" "----"

declare -A FILE_IDS

for e in "${UPLOAD_SIZES[@]}"; do
    l="${e%%:*}"; b="${e##*:}"; f="$WORKDIR/test_${l}.bin"
    login >/dev/null 2>&1
    rss0=$(get_rss_kb)

    start_monitor
    t0=$(date +%s%N)
    resp=$(curl -s -w '\n%{http_code}' --max-time 3600 \
      -X POST "$BASE_URL/api/files/upload" \
      -H "Authorization: Bearer $TOKEN" \
      -F "folder_id=$FOLDER_ID" \
      -F "file=@$f" 2>&1)
    t1=$(date +%s%N)
    stop_monitor

    code=$(echo "$resp" | tail -1)
    body=$(echo "$resp" | head -n -1)
    ms=$(( (t1 - t0) / 1000000 ))
    rss1=$(get_rss_kb)
    tp=$(throughput "$b" "$ms")
    fid=$(echo "$body" | python3 -c "import sys,json;print(json.load(sys.stdin)['id'])" 2>/dev/null || echo "")
    FILE_IDS[$l]="$fid"

    printf "%-8s │ %6dms %8s/s │ %5dMB %5dMB %5dMB %4dMB │ %5s │ %4s\n" \
           "$l" "$ms" "$tp" \
           $(mb $rss0) $(mb $MON_PEAK_RSS) $(mb $rss1) $(mb $MON_PEAK_SWAP) \
           "$MON_CPU_PCT" "$code"

    tpm=$(echo "scale=2;$b*1000/($ms+1)/1048576" | bc 2>/dev/null || echo 0)
    echo "upload_$l,$b,upload,$ms,$(mb $rss0),$(mb $MON_PEAK_RSS),$(mb $rss1),$(mb $MON_PEAK_SWAP),$MON_CPU_PCT,$tpm,$code" >> "$RESULTS"
    sleep 1
done

# ============================================================================
#  INJECT LARGE BLOBS (for download-only tests)
# ============================================================================
header "Injecting Large Blobs (bypass upload handler)"
for e in "${DOWNLOAD_ONLY_SIZES[@]}"; do
    l="${e%%:*}"; b="${e##*:}"
    fid=$(inject_blob "$l" "$b")
    FILE_IDS[$l]="$fid"
    ok "Registered $l → $fid"
done

# ============================================================================
#  DOWNLOADS  (all sizes including injected blobs)
# ============================================================================
ALL_SIZES=("${UPLOAD_SIZES[@]}" "${DOWNLOAD_ONLY_SIZES[@]}")

header "Download Benchmarks"
printf "${BLD}%-8s │ %8s %10s │ %7s %7s %7s %6s │ %5s │ %4s${NC}\n" \
       "Size" "Latency" "Thruput" "RSSbef" "RSSpk" "RSSaft" "Swap" "CPU%" "HTTP"
printf "%-8s │ %8s %10s │ %7s %7s %7s %6s │ %5s │ %4s\n" \
       "------" "-------" "--------" "------" "-----" "------" "----" "----" "----"

for e in "${ALL_SIZES[@]}"; do
    l="${e%%:*}"; b="${e##*:}"
    fid="${FILE_IDS[$l]:-}"
    [[ -z "$fid" ]] && { warn "$l skipped (no ID)"; continue; }

    login >/dev/null 2>&1
    rss0=$(get_rss_kb)

    start_monitor
    t0=$(date +%s%N)
    code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 7200 \
      "$BASE_URL/api/files/$fid" -H "Authorization: Bearer $TOKEN" 2>&1)
    t1=$(date +%s%N)
    stop_monitor

    ms=$(( (t1 - t0) / 1000000 ))
    rss1=$(get_rss_kb)
    tp=$(throughput "$b" "$ms")

    printf "%-8s │ %6dms %8s/s │ %5dMB %5dMB %5dMB %4dMB │ %5s │ %4s\n" \
           "$l" "$ms" "$tp" \
           $(mb $rss0) $(mb $MON_PEAK_RSS) $(mb $rss1) $(mb $MON_PEAK_SWAP) \
           "$MON_CPU_PCT" "$code"

    tpm=$(echo "scale=2;$b*1000/($ms+1)/1048576" | bc 2>/dev/null || echo 0)
    echo "download_$l,$b,download,$ms,$(mb $rss0),$(mb $MON_PEAK_RSS),$(mb $rss1),$(mb $MON_PEAK_SWAP),$MON_CPU_PCT,$tpm,$code" >> "$RESULTS"
    sleep 1
done

# ============================================================================
#  RANGE REQUESTS (first 64 KB — constant memory regardless of file size)
# ============================================================================
header "Range Requests (64 KB slice from each file)"
printf "${BLD}%-8s │ %8s │ %7s %7s │ %4s${NC}\n" "Size" "Latency" "RSSpk" "RSSdelta" "HTTP"
printf "%-8s │ %8s │ %7s %7s │ %4s\n" "------" "-------" "-----" "-------" "----"

for e in "${ALL_SIZES[@]}"; do
    l="${e%%:*}"; b="${e##*:}"
    fid="${FILE_IDS[$l]:-}"
    [[ -z "$fid" ]] && continue

    login >/dev/null 2>&1
    rss0=$(get_rss_kb)

    start_monitor
    t0=$(date +%s%N)
    code=$(curl -s -o /dev/null -w '%{http_code}' \
      "$BASE_URL/api/files/$fid" \
      -H "Authorization: Bearer $TOKEN" \
      -H "Range: bytes=0-65535" 2>&1)
    t1=$(date +%s%N)
    stop_monitor

    ms=$(( (t1 - t0) / 1000000 ))
    rss1=$(get_rss_kb)
    d=$(( rss1 - rss0 ))

    printf "%-8s │ %6dms │ %5dMB %+5dKB │ %4s\n" \
           "$l" "$ms" $(mb $MON_PEAK_RSS) "$d" "$code"

    echo "range_$l,$b,range,$ms,$(mb $rss0),$(mb $MON_PEAK_RSS),$(mb $rss1),$(mb $MON_PEAK_SWAP),$MON_CPU_PCT,0,$code" >> "$RESULTS"
done

# ============================================================================
#  CONCURRENT DOWNLOADS
# ============================================================================
header "Concurrent Downloads"

run_concurrent() {
    local lbl="$1" fid="$2" n="$3" each="$4"
    local total=$(( each * n ))
    login >/dev/null 2>&1
    rss0=$(get_rss_kb)
    start_monitor
    t0=$(date +%s%N)
    local pids=()
    for i in $(seq 1 $n); do
        curl -s -o /dev/null --max-time 7200 "$BASE_URL/api/files/$fid" \
          -H "Authorization: Bearer $TOKEN" &
        pids+=($!)
    done
    wait "${pids[@]}"
    t1=$(date +%s%N)
    stop_monitor
    ms=$(( (t1 - t0) / 1000000 ))
    rss1=$(get_rss_kb)
    tp=$(throughput $total $ms)
    printf "  ${BLD}%dx %s${NC}  %dms  %s/s  RSS:%dMB→peak %dMB→%dMB  Swap:%dMB  CPU:%s%%\n" \
           "$n" "$lbl" "$ms" "$tp" \
           $(mb $rss0) $(mb $MON_PEAK_RSS) $(mb $rss1) $(mb $MON_PEAK_SWAP) "$MON_CPU_PCT"
    echo "concurrent_${n}x${lbl},$total,concurrent,$ms,$(mb $rss0),$(mb $MON_PEAK_RSS),$(mb $rss1),$(mb $MON_PEAK_SWAP),$MON_CPU_PCT,0,200" >> "$RESULTS"
}

fid="${FILE_IDS[10MB]:-}";  [[ -n "$fid" ]] && run_concurrent "10MB"  "$fid" 5 10485760
fid="${FILE_IDS[100MB]:-}"; [[ -n "$fid" ]] && run_concurrent "100MB" "$fid" 3 104857600
fid="${FILE_IDS[1GB]:-}";   [[ -n "$fid" ]] && run_concurrent "1GB"   "$fid" 2 1073741824
fid="${FILE_IDS[5GB]:-}";   [[ -n "$fid" ]] && run_concurrent "5GB"   "$fid" 2 5368709120

# ============================================================================
#  MEMORY TRACE during 5 GB download
# ============================================================================
header "Memory Trace: RSS during 5 GB download"
fid="${FILE_IDS[5GB]:-}"
if [[ -n "$fid" ]]; then
    login >/dev/null 2>&1
    TRACE="$WORKDIR/mem_trace_5gb.csv"
    echo "elapsed_ms,rss_mb,vmsize_mb,swap_mb" > "$TRACE"
    t0=$(date +%s%N)
    (
        while true; do
            now=$(date +%s%N)
            el=$(( (now - t0) / 1000000 ))
            echo "$el,$(mb $(get_rss_kb)),$(mb $(get_vsize_kb)),$(mb $(get_swap_kb))" >> "$TRACE"
            sleep 0.2
        done
    ) &
    SAMPLER=$!
    curl -s -o /dev/null --max-time 7200 "$BASE_URL/api/files/$fid" -H "Authorization: Bearer $TOKEN"
    kill $SAMPLER 2>/dev/null; wait $SAMPLER 2>/dev/null || true

    mx=$(awk -F, 'NR>1{if($2>m)m=$2}END{print m+0}' "$TRACE")
    mn=$(awk -F, 'NR>1{if(!m||$2<m)m=$2}END{print m+0}' "$TRACE")
    samp=$(( $(wc -l < "$TRACE") - 1 ))
    printf "  Samples:    %d (every 200ms)\n" "$samp"
    printf "  RSS min:    %d MB\n" "$mn"
    printf "  RSS max:    %d MB\n" "$mx"
    printf "  RSS delta:  %d MB\n" $(( mx - mn ))
    printf "  Trace file: %s\n" "$TRACE"
else
    warn "5GB file not available"
fi

# ============================================================================
#  FINAL STATE
# ============================================================================
header "Final Server State"
printf "  RSS:     %5d MB\n"  $(mb $(get_rss_kb))
printf "  VmPeak:  %5d MB\n"  $(mb $(get_peak_kb))
printf "  VmSize:  %5d MB\n"  $(mb $(get_vsize_kb))
printf "  VmSwap:  %5d MB\n"  $(mb $(get_swap_kb))
echo
printf "${BLD}CSV Results:${NC}\n"
column -t -s',' "$RESULTS" 2>/dev/null || cat "$RESULTS"
echo
ok "Benchmark v3 complete — $RESULTS"
