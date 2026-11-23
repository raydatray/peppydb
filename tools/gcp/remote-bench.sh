#!/bin/sh
# The startup script of a peppydb bench VM (plan 0024). tools/gcp-bench
# passes it as the instance's startup-script metadata. Do not run it by hand
# on a machine you care about: it formats the Local SSD.
#
# On boot it:
#   1. reads peppydb-bucket and peppydb-run-id from the metadata server;
#   2. exits if this VM already started a run (startup scripts run on every
#      boot; after a reboot in the middle of a run it reports the run as
#      failed), and otherwise uploads results/RUN/started;
#   3. installs tools from the Ubuntu archive (the only thing that comes
#      from the internet);
#   4. formats the Local SSD as ext4 and mounts it at /mnt/bench;
#   5. downloads artifacts/RUN/ from GCS and checks every sha256 in MANIFEST;
#   6. writes env.txt;
#   7. runs the suite, one step at a time, uploading each step's output;
#   8. uploads status and startup-script.log, then DONE.
#
# Test overrides (tools/gcp/testing/test-remote.sh):
#   PEPPYDB_REMOTE_STATE     state and work directory (/var/lib/peppydb-bench)
#   PEPPYDB_REMOTE_METADATA  directory of fake metadata answers, one file per
#                            path with "/" replaced by "_"
#   PEPPYDB_REMOTE_NO_MKFS=1 skip the disk step; PEPPYDB_BENCH_DIR must be set
#   PEPPYDB_REMOTE_DISK      the Local SSD device
set -u
export PATH="$PATH:/snap/bin:/usr/sbin:/sbin"

STATE=${PEPPYDB_REMOTE_STATE:-/var/lib/peppydb-bench}
ARTIFACTS=$STATE/artifacts
RESULTS=$STATE/results
LOG=$STATE/startup-script.log
# Seconds a step may run before its watchdog kills the test binary, so a
# hung benchmark fails its step instead of holding the VM until --max-run.
# PEPPYDB_BENCH_STEP_SECONDS (in bench.env) overrides it.
STEP_SECONDS=1200
# binutils provides addr2line, which perf report and perf script need for
# inlined frames and source lines, and which fold-stacks.py uses.
PACKAGES="linux-tools-common binutils bpftrace sysstat nvme-cli zstd"

log() {
    echo "remote-bench: $(date -u +%Y-%m-%dT%H:%M:%SZ) $*"
}

# metadata PATH: one value from the metadata server, relative to
# computeMetadata/v1/.
metadata() {
    if [ -n "${PEPPYDB_REMOTE_METADATA:-}" ]; then
        cat "$PEPPYDB_REMOTE_METADATA/$(echo "$1" | tr / _)" 2>/dev/null
    else
        curl -fsS --retry 5 -H 'Metadata-Flavor: Google' \
            "http://metadata.google.internal/computeMetadata/v1/$1"
    fi
}

# gcs_cp SRC DST: one object, with retries.
gcs_cp() {
    for attempt in 1 2 3; do
        if gcloud storage cp --quiet "$1" "$2" >/dev/null 2>&1; then
            return 0
        fi
        sleep "$attempt"
    done
    log "copy failed: $1 -> $2"
    return 1
}

# upload FILE...: each file to results/RUN/ under its own name.
upload() {
    for file in "$@"; do
        [ -f "$file" ] && gcs_cp "$file" "$REMOTE_RESULTS/$(basename "$file")"
    done
    return 0
}

# finish STATUS REASON: write and upload the final files, DONE last.
finish() {
    {
        echo "status=$1"
        echo "reason=$2"
        echo "failed_steps=$(awk '$3 != 0 { printf "%s%s:%s", sep, $1, $2; sep = "," }' "$RESULTS/status" 2>/dev/null)"
    } >"$RESULTS/DONE"
    log "finished: status=$1 reason=$2"
    touch "$RESULTS/status"
    # The local file list shows whether a missing result was never written
    # or failed to upload (the copy failures are in the log).
    ls -l "$RESULTS" >"$RESULTS/files.txt" 2>&1
    upload "$RESULTS/env.txt" "$RESULTS/status" "$RESULTS/files.txt" "$LOG"
    upload "$RESULTS/DONE"
}

# install_tools: tools from the Ubuntu archive. Background jobs are stopped
# first: they hold the package lock after boot and must not run during a
# benchmark.
install_tools() {
    systemctl stop unattended-upgrades.service apt-daily.timer \
        apt-daily-upgrade.timer apt-daily.service apt-daily-upgrade.service
    snap refresh --hold
    apt-get -o DPkg::Lock::Timeout=600 update -q &&
        DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=600 \
            install -y -q --no-install-recommends \
            "linux-tools-$(uname -r)" $PACKAGES
}

# setup_disk: a fresh ext4 file system on the Local SSD at /mnt/bench.
setup_disk() {
    if [ "${PEPPYDB_REMOTE_NO_MKFS:-}" = 1 ]; then
        [ -n "${PEPPYDB_BENCH_DIR:-}" ] && mkdir -p "$PEPPYDB_BENCH_DIR"
        return
    fi
    disk=${PEPPYDB_REMOTE_DISK:-/dev/disk/by-id/google-local-nvme-ssd-0}
    waited=0
    while [ ! -e "$disk" ] && [ "$waited" -lt 60 ]; do
        sleep 1
        waited=$((waited + 1))
    done
    [ -e "$disk" ] || {
        log "no Local SSD at $disk"
        return 1
    }
    mkfs.ext4 -F -q "$disk" &&
        mkdir -p /mnt/bench &&
        mount "$disk" /mnt/bench &&
        PEPPYDB_BENCH_DIR=/mnt/bench
}

# download_artifacts: MANIFEST first, then every file it lists, each
# checked against its sha256. MANIFEST lines: "sha256 PATH HEX".
download_artifacts() {
    mkdir -p "$ARTIFACTS" &&
        gcs_cp "$REMOTE_ARTIFACTS/MANIFEST" "$ARTIFACTS/MANIFEST" || return 1
    status=0
    while read -r kind path hash; do
        [ "$kind" = sha256 ] || continue
        case $path in
            */../* | ../* | */.. | /*) log "bad path in MANIFEST: $path"; return 1 ;;
        esac
        mkdir -p "$ARTIFACTS/$(dirname "$path")"
        gcs_cp "$REMOTE_ARTIFACTS/$path" "$ARTIFACTS/$path" || return 1
        actual=$(sha256sum "$ARTIFACTS/$path" | cut -d' ' -f1)
        if [ "$actual" != "$hash" ]; then
            log "checksum mismatch: $path"
            status=1
        fi
    done <"$ARTIFACTS/MANIFEST"
    [ "$status" = 0 ] || return 1
    [ -f "$ARTIFACTS/suite.txt" ] || {
        log "MANIFEST lists no suite.txt"
        return 1
    }
    [ ! -d "$ARTIFACTS/bin" ] || chmod +x "$ARTIFACTS"/bin/*
}

# manifest_value KEY: a "KEY=VALUE" line of MANIFEST.
manifest_value() {
    sed -n "s/^$1=//p" "$ARTIFACTS/MANIFEST" | head -n 1
}

# load_bench_env: PEPPYDB_BENCH_* settings from bench.env (one KEY=VALUE
# per line). Other keys and unusual values are refused.
load_bench_env() {
    [ -f "$ARTIFACTS/bench.env" ] || return 0
    while IFS= read -r line; do
        case $line in
            '' | '#'*) continue ;;
        esac
        key=${line%%=*}
        value=${line#*=}
        if ! echo "$key" | grep -Eq '^PEPPYDB_BENCH_[A-Z0-9_]+$' ||
            ! echo "$value" | grep -Eq '^[A-Za-z0-9._-]*$'; then
            log "refused bench.env line: $line"
            return 1
        fi
        export "$key=$value"
    done <"$ARTIFACTS/bench.env"
}

section() {
    echo
    echo "## $1"
}

write_env() {
    {
        echo "# peppydb bench environment (plan 0024)"
        echo "run_id=$RUN_ID"
        echo "virtual_machine=yes"
        echo "machine_type=$(metadata instance/machine-type | sed 's|.*/||')"
        echo "zone=$(metadata instance/zone | sed 's|.*/||')"
        echo "image=$(metadata instance/image | sed 's|.*/||')"
        echo "preemptible=$(metadata instance/scheduling/preemptible)"
        echo "kernel=$(uname -r)"
        echo "build_mode=$(manifest_value build_mode)"
        echo "revision=$(manifest_value revision)"
        echo "dirty=$(manifest_value dirty)"
        echo "bench_dir=${PEPPYDB_BENCH_DIR:-}"
        env | grep '^PEPPYDB_BENCH_' | sort
        echo "tools_installed=$TOOLS"
        section "MANIFEST"
        cat "$ARTIFACTS/MANIFEST"
        section "uname -a"
        uname -a
        section "/etc/os-release"
        cat /etc/os-release
        section "lscpu"
        lscpu
        section "nproc"
        nproc
        section "free -g"
        free -g
        section "findmnt bench_dir"
        findmnt -T "${PEPPYDB_BENCH_DIR:-/}"
        section "lsblk"
        lsblk -o NAME,MODEL,SIZE,ROTA,TYPE,MOUNTPOINT
        section "tool versions"
        for package in "linux-tools-$(uname -r)" $PACKAGES; do
            dpkg-query -W -f '${Package} ${Version}\n' "$package" 2>/dev/null ||
                echo "$package not installed"
        done
        if command -v nvme >/dev/null 2>&1; then
            section "nvme list"
            nvme list
        fi
        env_extra
    } >"$RESULTS/env.txt" 2>&1
}

# --- Profilers. C4D exposes no hardware counters (no PMU), so every
# profiler uses software events and tracepoints.
PROFILERS="none stat record record-dwarf offcpu sched"

known_profiler() {
    for known in $PROFILERS; do
        [ "$1" = "$known" ] && return 0
    done
    return 1
}

# pmu_state: "available" if perf can count CPU cycles.
pmu_state() {
    if command -v perf >/dev/null 2>&1 &&
        perf stat -x, -e cycles true 2>&1 | grep -q '^[0-9]'; then
        echo available
    else
        echo unavailable
    fi
}

env_extra() {
    echo "profile=$PROFILE"
    echo "pmu=$(pmu_state)"
    for setting in kernel.perf_event_paranoid kernel.kptr_restrict; do
        echo "$setting=$(sysctl -n "$setting" 2>/dev/null)"
    done
    if command -v perf >/dev/null 2>&1; then
        section "perf version"
        perf version
    fi
    if command -v bpftrace >/dev/null 2>&1; then
        section "bpftrace version"
        bpftrace --version
    fi
}

# prepare_profiling: kernel settings and tool checks for the chosen
# profilers.
prepare_profiling() {
    [ "$PROFILE" = none ] && return 0
    # Kernel frames and addresses in profiles. A failure is recorded in
    # env.txt but does not stop the run.
    sysctl -q -w kernel.perf_event_paranoid=-1 kernel.kptr_restrict=0 ||
        log "warning: cannot set perf sysctls"
    for profiler in $(echo "$PROFILE" | tr , ' '); do
        case $profiler in
            stat | record | record-dwarf | sched)
                perf version >/dev/null 2>&1 || {
                    log "perf does not run"
                    return 1
                }
                ;;
            offcpu)
                command -v bpftrace >/dev/null 2>&1 &&
                    [ -f "$ARTIFACTS/offcpu.bt" ] && [ -f "$ARTIFACTS/fold-stacks.py" ] || {
                    log "offcpu needs bpftrace, offcpu.bt, and fold-stacks.py"
                    return 1
                }
                ;;
        esac
    done
}

# compress FILE: FILE.zst in place of FILE; prints the name to upload.
# If zstd fails, the uncompressed file is uploaded instead.
compress() {
    [ -f "$1" ] || {
        log "missing output: $1"
        return 0
    }
    if zstd -q -f --rm "$1" -o "$1.zst"; then
        echo "$1.zst"
    else
        log "zstd failed on $1; uploading it uncompressed"
        rm -f "$1.zst"
        echo "$1"
    fi
}

# perf_text OUT LOG ARGS...: run "perf ARGS" into OUT, with stderr in LOG.
# perf report and perf script resolve inlined frames with addr2line; if
# that fails, run again with --no-inline, which loses inlined frames but
# keeps the samples.
perf_text() {
    out=$1 err=$2
    shift 2
    echo "perf $*" >>"$err"
    if perf "$@" >"$out" 2>>"$err" && ! grep -q 'addr2line configuration failed' "$out"; then
        return 0
    fi
    log "perf $1 failed; retrying with --no-inline (see $(basename "$err"))"
    echo "perf $* --no-inline" >>"$err"
    perf "$@" --no-inline >"$out" 2>>"$err"
}

# watch_maps PATTERN OUT: until killed, copy the memory map of the newest
# process whose command line starts with PATTERN to OUT. fold-stacks.py
# needs it to symbolize the addresses that bpftrace could not resolve
# because the process had exited.
watch_maps() {
    while :; do
        pid=$(pgrep -n -f "^$1" 2>/dev/null)
        if [ -n "$pid" ] && cp "/proc/$pid/maps" "$2.tmp" 2>/dev/null &&
            [ -s "$2.tmp" ]; then
            mv "$2.tmp" "$2"
        fi
        sleep 1
    done
}

# run_profiled PROFILER NAME EXE ARGS...: run one step under one profiler.
# The test output goes to RESULTS/NAME.log (no profiler) or
# RESULTS/NAME.PROFILER.log. Prints the files to upload, one per line, on
# fd 3. Returns the step's exit code. If EXE is a launcher that execs the
# real binary, MAPS_EXE names that binary for the offcpu memory map.
run_profiled() {
    profiler=$1 name=$2 exe=$3
    shift 3
    base=$RESULTS/$name
    log_file=$(step_log "$name" "$profiler")
    echo "$log_file" >&3
    case $profiler in
        none)
            "$exe" "$@" >"$log_file" 2>&1
            ;;
        stat)
            perf stat -x, -o "$base.stat.csv" \
                -e task-clock,context-switches,cpu-migrations,page-faults \
                -- "$exe" "$@" >"$log_file" 2>&1
            rc=$?
            echo "$base.stat.csv" >&3
            return "$rc"
            ;;
        record | record-dwarf)
            call_graph=fp
            [ "$profiler" = record-dwarf ] && call_graph=dwarf
            data=$base.$profiler.perf.data
            perf record -q -e cpu-clock -F 999 --call-graph "$call_graph" -o "$data" \
                -- "$exe" "$@" >"$log_file" 2>&1
            rc=$?
            perf_log=$base.$profiler.perf.log
            : >"$perf_log"
            perf_text "$base.$profiler.report.txt" "$perf_log" \
                report -i "$data" --stdio --no-children --percent-limit 0.5
            perf_text "$base.$profiler.perf-script.txt" "$perf_log" script -i "$data"
            echo "$base.$profiler.report.txt" >&3
            echo "$perf_log" >&3
            compress "$base.$profiler.perf-script.txt" >&3
            compress "$data" >&3
            return "$rc"
            ;;
        offcpu)
            # bpftrace -c splits the command on spaces, as the suite does.
            maps=$base.offcpu.maps
            rm -f "$maps"
            # Not on fd 3 or stdout: the caller reads them to the end.
            watch_maps "${MAPS_EXE:-$exe}" "$maps" </dev/null >/dev/null 2>&1 3>&- &
            watcher=$!
            bpftrace -q -o "$base.offcpu.bpftrace.txt" -c "$exe $*" "$ARTIFACTS/offcpu.bt" \
                >"$log_file" 2>&1
            rc=$?
            kill "$watcher" 2>/dev/null
            wait "$watcher" 2>/dev/null
            if [ -s "$maps" ]; then
                python3 "$ARTIFACTS/fold-stacks.py" --maps "$maps"
            else
                log "no memory map for ${MAPS_EXE:-$exe}; hexadecimal frames stay unresolved"
                python3 "$ARTIFACTS/fold-stacks.py"
            fi <"$base.offcpu.bpftrace.txt" >"$base.offcpu.folded" 2>>"$log_file"
            echo "$base.offcpu.folded" >&3
            [ -f "$maps" ] && echo "$maps" >&3
            compress "$base.offcpu.bpftrace.txt" >&3
            return "$rc"
            ;;
        sched)
            data=$base.sched.perf.data
            perf sched record -q -o "$data" -- "$exe" "$@" >"$log_file" 2>&1
            rc=$?
            {
                perf sched -i "$data" latency --sort max
                perf sched -i "$data" timehist --summary
            } >"$base.sched.txt" 2>&1
            echo "$base.sched.txt" >&3
            compress "$data" >&3
            return "$rc"
            ;;
    esac
}

# watchdog EXE SECONDS MARKER: after SECONDS, kill every process whose
# command line starts with EXE (the test binary, not the profiler around
# it, so the profiler still writes its output) and create MARKER if one was
# killed. The caller kills the watchdog when the step ends. It sleeps one
# second at a time, so it never outlives its step by more than that.
watchdog() {
    deadline=$(($(date +%s) + $2))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        sleep 1
    done
    if pkill -KILL -f "^$1"; then
        : >"$3"
    fi
}

# step_log NAME PROFILER: the file with the step's test output.
step_log() {
    if [ "$2" = none ]; then
        echo "$RESULTS/$1.log"
    else
        echo "$RESULTS/$1.$2.log"
    fi
}

# check_output NAME LOG: libtest output checks. Every step must run at least
# one test, and no test may fail (some profilers do not pass on the exit
# code). A preflight step fails if a test reports a skip.
check_output() {
    if grep -q '^running 0 tests' "$2"; then
        echo "no tests matched"
        return 1
    fi
    if grep -q '^test result: FAILED' "$2"; then
        echo "tests failed"
        return 1
    fi
    case $1 in
        preflight-*)
            if grep -q 'skipped:' "$2"; then
                echo "a test was skipped"
                return 1
            fi
            ;;
    esac
    return 0
}

# run_suite: every step of suite.txt under every profiler in order. A line
# is "NAME BINARY ARGS...", split on whitespace (no quoting); "#" starts a
# comment line. Returns 1 if a preflight step failed and 2 if the suite or
# the profiler list is invalid.
run_suite() {
    : >"$RESULTS/status"
    profilers=$(echo "$PROFILE" | tr , ' ')
    for profiler in $profilers; do
        known_profiler "$profiler" || {
            log "unknown profiler: $profiler"
            return 2
        }
    done
    # Read the suite on fd 4 so that steps cannot consume it from stdin.
    while read -r name bin args <&4; do
        case $name in
            '' | '#'*) continue ;;
        esac
        if ! echo "$name" | grep -Eq '^[a-z0-9][a-z0-9-]*$' ||
            [ ! -x "$ARTIFACTS/bin/$bin" ]; then
            log "bad suite line: $name $bin"
            echo "$name - 2 0 bad-suite-line" >>"$RESULTS/status"
            return 2
        fi
        # Preflight steps run once, without a profiler.
        step_profilers=$profilers
        case $name in
            preflight-*) step_profilers=none ;;
        esac
        for profiler in $step_profilers; do
            log "step $name ($profiler): $bin $args"
            started=$(date +%s)
            timed_out=$STATE/step-timed-out
            rm -f "$timed_out"
            # Not on fd 3 or stdout: the caller reads them to the end.
            watchdog "$ARTIFACTS/bin/$bin" "$STEP_SECONDS" "$timed_out" \
                </dev/null >/dev/null 2>&1 3>&- &
            dog=$!
            # shellcheck disable=SC2086 # args split on whitespace by design
            files=$(run_profiled "$profiler" "$name" "$ARTIFACTS/bin/$bin" $args 3>&1 </dev/null)
            rc=$?
            kill "$dog" 2>/dev/null
            wait "$dog" 2>/dev/null
            elapsed=$(($(date +%s) - started))
            note=ok
            [ "$rc" = 0 ] || note=exit-$rc
            if [ -e "$timed_out" ]; then
                rc=124
                note=timeout-${STEP_SECONDS}s
            elif problem=$(check_output "$name" "$(step_log "$name" "$profiler")"); then
                :
            else
                [ "$rc" = 0 ] && rc=1
                note=$(echo "$problem" | tr ' ' -)
            fi
            echo "$name $profiler $rc $elapsed $note" >>"$RESULTS/status"
            log "step $name ($profiler): $note after ${elapsed}s"
            # shellcheck disable=SC2086 # one file per line
            upload $files "$RESULTS/status"
            case $name in
                preflight-*) [ "$rc" = 0 ] || return 1 ;;
            esac
        done
    done 4<"$ARTIFACTS/suite.txt"
}

# gcs_exists URL: true if the object exists in GCS.
gcs_exists() {
    gcloud storage ls "$1" >/dev/null 2>&1
}

# wait_tcp HOST PORT PID: wait up to 30s for HOST:PORT to accept TCP
# connections while PID stays alive.
wait_tcp() {
    [ "${PEPPYDB_REMOTE_SKIP_TCP_WAIT:-}" = 1 ] && return 0
    host=$1
    port=$2
    pid=$3
    waited=0
    while [ "$waited" -lt 60 ]; do
        if ! kill -0 "$pid" 2>/dev/null; then
            log "server process $pid exited before listening on $host:$port"
            return 1
        fi
        if python3 -c "import socket, sys; s = socket.create_connection((sys.argv[1], int(sys.argv[2])), 0.5); s.close()" "$host" "$port" 2>/dev/null; then
            return 0
        fi
        sleep 0.5
        waited=$((waited + 1))
    done
    log "timed out waiting for $host:$port"
    return 1
}

# expand_args TEXT: substitute $BENCH_DIR, $SERVER_ADDR, $METRICS_ADDR,
# $CLUSTER_ADDRS, $NODE_ID, $PEERS, and $BOOTSTRAP.
expand_args() {
    printf '%s' "$1" | sed \
        -e "s|\\\$BENCH_DIR|${PEPPYDB_BENCH_DIR:-}|g" \
        -e "s|\\\$SERVER_ADDR|${SERVER_ADDR:-}|g" \
        -e "s|\\\$METRICS_ADDR|${METRICS_ADDR:-}|g" \
        -e "s|\\\$CLUSTER_ADDRS|${CLUSTER_ADDRS:-}|g" \
        -e "s|\\\$NODE_ID|${NODE_ID:-}|g" \
        -e "s|\\\$PEERS|${PEERS:-}|g" \
        -e "s|\\\$BOOTSTRAP|${BOOTSTRAP:-}|g"
}

# run_server_role: start the server line from suite.txt under the run's
# profiler, publish ready with our internal IP, wait for client/DONE, then
# stop the server cleanly. In cluster mode (ROLE=server-N, CLUSTER_SIZE>1),
# announces its IP first and gathers all peers' IPs before starting the
# server. The server runs once for the whole client suite, so a multi-VM run
# takes one profiler. The profiler runs a launcher that records its PID and
# then execs the server, so the server keeps the PID that the profiler
# started (offcpu.bt traces that PID). SIGTERM goes to the server, not to the
# profiler, so the profiler writes its output after the server exits.
run_server_role() {
    : >"$RESULTS/status"
    case $PROFILE in
        *,*)
            log "a client-server run takes one profiler, not $PROFILE"
            echo "server - 2 0 one-profiler-only" >>"$RESULTS/status"
            return 2
            ;;
    esac
    known_profiler "$PROFILE" || {
        log "unknown profiler: $PROFILE"
        echo "server - 2 0 unknown-profiler" >>"$RESULTS/status"
        return 2
    }
    server_line=$(awk '$1 == "server" { $1 = ""; sub(/^ /, ""); print; exit }' "$ARTIFACTS/suite.txt")
    if [ -z "$server_line" ]; then
        log "suite.txt has no server line"
        return 2
    fi
    set -- $server_line
    bin=$1
    shift
    raw_args=$*
    exe=$ARTIFACTS/bin/$bin
    if [ ! -x "$exe" ]; then
        log "server binary not executable: $bin"
        return 2
    fi
    listen_port=7420
    metrics_port=9090
    prev=
    for tok in $raw_args; do
        case $prev in
            --listen) listen_port=${tok##*:} ;;
            --metrics-listen) metrics_port=${tok##*:} ;;
        esac
        prev=$tok
    done

    ip=$(metadata instance/network-interfaces/0/ip 2>/dev/null || true)
    ip=${ip:-127.0.0.1}
    poll=${PEPPYDB_REMOTE_POLL:-5}

    if [ "$CLUSTER_SIZE" -gt 1 ]; then
        NODE_ID=${ROLE#server-}
        BOOTSTRAP=
        [ "$NODE_ID" = 1 ] && BOOTSTRAP=--bootstrap
        echo "ip=$ip" >"$RESULTS/ip"
        upload "$RESULTS/ip"
        log "cluster node $NODE_ID/$CLUSTER_SIZE announced ip=$ip; waiting for peers"
        PEERS=
        i=1
        while [ "$i" -le "$CLUSTER_SIZE" ]; do
            peer_results="$REMOTE_RESULTS_ROOT/server-$i"
            while :; do
                if gcs_exists "$peer_results/ip"; then
                    break
                fi
                if gcs_exists "$peer_results/DONE"; then
                    log "peer server-$i finished before announcing IP"
                    return 1
                fi
                sleep "$poll"
            done
            gcs_cp "$peer_results/ip" "$STATE/server-$i.ip" || return 1
            peer_ip=$(sed -n 's/^ip=//p' "$STATE/server-$i.ip")
            sep=
            [ -n "$PEERS" ] && sep=,
            PEERS="${PEERS}${sep}${i}=${peer_ip}:${listen_port}"
            i=$((i + 1))
        done
        export NODE_ID PEERS BOOTSTRAP
        log "cluster peers: $PEERS (bootstrap='${BOOTSTRAP}')"
    fi

    args=$(expand_args "$raw_args")
    pid_file=$STATE/server.pid
    launcher=$STATE/server-launch
    server_files=$STATE/server.files
    rm -f "$pid_file" "$server_files"
    {
        echo '#!/bin/sh'
        echo "echo \$\$ >\"$pid_file\""
        echo "exec \"$exe\" \"\$@\""
    } >"$launcher" && chmod +x "$launcher" || return 2
    started=$(date +%s)
    log "starting server ($PROFILE): $bin $args"
    # shellcheck disable=SC2086 # args split on whitespace by design
    MAPS_EXE=$exe run_profiled "$PROFILE" server "$launcher" $args \
        3>"$server_files" </dev/null &
    runner=$!

    if ! wait_tcp 127.0.0.1 "$listen_port" "$runner"; then
        stop_server "$pid_file" "$runner"
        elapsed=$(($(date +%s) - started))
        echo "$ROLE $PROFILE 1 $elapsed failed-to-listen" >>"$RESULTS/status"
        # shellcheck disable=SC2046 # one file per line
        upload $(cat "$server_files") "$RESULTS/status"
        return 1
    fi

    {
        echo "ip=$ip"
        echo "port=$listen_port"
        echo "metrics_port=$metrics_port"
    } >"$RESULTS/ready"
    upload "$RESULTS/ready"
    log "$ROLE ready at $ip:$listen_port (metrics :$metrics_port)"

    while :; do
        if gcs_exists "$REMOTE_CLIENT_RESULTS/DONE"; then
            log "client finished; stopping server"
            break
        fi
        if ! kill -0 "$runner" 2>/dev/null; then
            wait "$runner"
            rc=$?
            elapsed=$(($(date +%s) - started))
            log "server exited early with status $rc"
            echo "$ROLE $PROFILE $rc $elapsed exited-early" >>"$RESULTS/status"
            # shellcheck disable=SC2046 # one file per line
            upload $(cat "$server_files") "$RESULTS/status"
            return 1
        fi
        sleep "$poll"
    done

    stop_server "$pid_file" "$runner"
    rc=$?
    elapsed=$(($(date +%s) - started))
    note=ok
    [ "$rc" = 0 ] || note=exit-$rc
    echo "$ROLE $PROFILE $rc $elapsed $note" >>"$RESULTS/status"
    log "$ROLE ($PROFILE): $note after ${elapsed}s"
    # shellcheck disable=SC2046 # one file per line
    upload $(cat "$server_files") "$RESULTS/status"
    return 0
}

# stop_server PID_FILE RUNNER: send SIGTERM to the server whose PID is in
# PID_FILE and SIGKILL if it still runs after 30 s, then wait for RUNNER
# (run_profiled in the background), which writes the profiler's outputs.
# Returns RUNNER's exit code.
stop_server() {
    pid=$(cat "$1" 2>/dev/null)
    if [ -n "$pid" ]; then
        kill -TERM "$pid" 2>/dev/null || true
        waited=0
        while kill -0 "$pid" 2>/dev/null && [ "$waited" -lt 30 ]; do
            sleep 1
            waited=$((waited + 1))
        done
        if kill -0 "$pid" 2>/dev/null; then
            log "server did not stop after SIGTERM; sending SIGKILL"
            kill -KILL "$pid" 2>/dev/null || true
        fi
    fi
    wait "$2"
}

# run_client_role: wait for server(s) to publish ready, then run every
# non-server step in suite.txt.
run_client_role() {
    : >"$RESULTS/status"
    poll=${PEPPYDB_REMOTE_POLL:-5}
    METRICS_ENDPOINTS=
    if [ "$CLUSTER_SIZE" -gt 1 ]; then
        CLUSTER_ADDRS=
        i=1
        while [ "$i" -le "$CLUSTER_SIZE" ]; do
            srv_results="$REMOTE_RESULTS_ROOT/server-$i"
            log "waiting for server-$i ready at $srv_results/ready"
            while :; do
                if gcs_exists "$srv_results/ready"; then
                    break
                fi
                if gcs_exists "$srv_results/DONE"; then
                    log "server-$i finished without becoming ready"
                    return 1
                fi
                sleep "$poll"
            done
            gcs_cp "$srv_results/ready" "$STATE/server-$i.ready" || return 1
            s_ip=$(sed -n 's/^ip=//p' "$STATE/server-$i.ready")
            s_port=$(sed -n 's/^port=//p' "$STATE/server-$i.ready")
            m_port=$(sed -n 's/^metrics_port=//p' "$STATE/server-$i.ready")
            s_addr="$s_ip:${s_port:-7420}"
            m_addr="$s_ip:${m_port:-9090}"
            sep=
            [ -n "$CLUSTER_ADDRS" ] && sep=,
            CLUSTER_ADDRS="${CLUSTER_ADDRS}${sep}${s_addr}"
            if [ "$i" = 1 ]; then
                SERVER_ADDR=$s_addr
                METRICS_ADDR=$m_addr
            fi
            METRICS_ENDPOINTS="$METRICS_ENDPOINTS server-$i=$m_addr"
            i=$((i + 1))
        done
        export CLUSTER_ADDRS SERVER_ADDR METRICS_ADDR
        log "connecting to cluster at $CLUSTER_ADDRS"
    else
        log "waiting for server ready at $REMOTE_SERVER_RESULTS/ready"
        while :; do
            if gcs_exists "$REMOTE_SERVER_RESULTS/ready"; then
                break
            fi
            if gcs_exists "$REMOTE_SERVER_RESULTS/DONE"; then
                log "server finished without becoming ready"
                return 1
            fi
            sleep "$poll"
        done
        gcs_cp "$REMOTE_SERVER_RESULTS/ready" "$STATE/server.ready" || return 1
        server_ip=$(sed -n 's/^ip=//p' "$STATE/server.ready")
        server_port=$(sed -n 's/^port=//p' "$STATE/server.ready")
        metrics_port=$(sed -n 's/^metrics_port=//p' "$STATE/server.ready")
        SERVER_ADDR="$server_ip:${server_port:-7420}"
        METRICS_ADDR="$server_ip:${metrics_port:-9090}"
        CLUSTER_ADDRS=$SERVER_ADDR
        METRICS_ENDPOINTS="server=$METRICS_ADDR"
        export CLUSTER_ADDRS SERVER_ADDR METRICS_ADDR
        log "connecting to server at $SERVER_ADDR (metrics $METRICS_ADDR)"
    fi

    while read -r name bin raw_args <&4; do
        case $name in
            '' | '#'*) continue ;;
            server) continue ;;
        esac
        if ! echo "$name" | grep -Eq '^[a-z0-9][a-z0-9-]*$' ||
            [ ! -x "$ARTIFACTS/bin/$bin" ]; then
            log "bad suite line: $name $bin"
            echo "$name - 2 0 bad-suite-line" >>"$RESULTS/status"
            return 2
        fi
        args=$(expand_args "$raw_args")
        log "step $name (none): $bin $args"
        started=$(date +%s)
        # shellcheck disable=SC2086 # args split on whitespace by design
        files=$(run_profiled none "$name" "$ARTIFACTS/bin/$bin" $args 3>&1 </dev/null)
        rc=$?
        elapsed=$(($(date +%s) - started))
        note=ok
        [ "$rc" = 0 ] || note=exit-$rc
        if problem=$(check_output "$name" "$(step_log "$name" none)"); then
            :
        else
            [ "$rc" = 0 ] && rc=1
            note=$(echo "$problem" | tr ' ' -)
        fi
        echo "$name none $rc $elapsed $note" >>"$RESULTS/status"
        log "step $name (none): $note after ${elapsed}s"
        if command -v curl >/dev/null 2>&1; then
            for pair in $METRICS_ENDPOINTS; do
                m_name=${pair%%=*}
                m_addr=${pair#*=}
                out_file="$RESULTS/$name.metrics.txt"
                [ "$CLUSTER_SIZE" -gt 1 ] && out_file="$RESULTS/$name.$m_name.metrics.txt"
                if curl -fsS --max-time 5 "http://$m_addr/metrics" >"$out_file" 2>/dev/null; then
                    files="$files $out_file"
                else
                    rm -f "$out_file"
                fi
            done
        fi
        # shellcheck disable=SC2086 # one file per line
        upload $files "$RESULTS/status"
    done 4<"$ARTIFACTS/suite.txt"
}

main() {
    BUCKET=$(metadata instance/attributes/peppydb-bucket)
    RUN_ID=$(metadata instance/attributes/peppydb-run-id)
    if [ -z "$BUCKET" ] || [ -z "$RUN_ID" ]; then
        log "peppydb-bucket or peppydb-run-id missing from metadata"
        return 1
    fi
    ROLE=$(metadata instance/attributes/peppydb-role 2>/dev/null || true)
    ROLE=${ROLE:-single}
    CLUSTER_SIZE=$(metadata instance/attributes/peppydb-cluster-size 2>/dev/null || true)
    CLUSTER_SIZE=${CLUSTER_SIZE:-1}
    REMOTE_ARTIFACTS=gs://$BUCKET/peppydb-bench/artifacts/$RUN_ID
    REMOTE_RESULTS_ROOT=gs://$BUCKET/peppydb-bench/results/$RUN_ID
    case $ROLE in
        single)
            REMOTE_RESULTS=$REMOTE_RESULTS_ROOT
            ;;
        server | server-*)
            REMOTE_RESULTS=$REMOTE_RESULTS_ROOT/$ROLE
            REMOTE_CLIENT_RESULTS=$REMOTE_RESULTS_ROOT/client
            ;;
        client)
            REMOTE_RESULTS=$REMOTE_RESULTS_ROOT/client
            REMOTE_SERVER_RESULTS=$REMOTE_RESULTS_ROOT/server
            ;;
        *)
            log "unknown peppydb-role: $ROLE"
            return 1
            ;;
    esac

    # Startup scripts run on every boot. After a reboot in the middle of a
    # run (for example, a host error), report the run as failed instead of
    # leaving the driver waiting.
    if [ -e "$STATE/started" ]; then
        if [ -e "$RESULTS/DONE" ]; then
            log "this VM already finished its run; nothing to do"
        else
            log "the VM rebooted during the run"
            finish failed "the VM rebooted during the run"
        fi
        return 0
    fi
    mkdir -p "$RESULTS" && date -u +%Y-%m-%dT%H:%M:%SZ >"$STATE/started" || return 1
    cp "$STATE/started" "$RESULTS/started"
    upload "$RESULTS/started"
    log "run $RUN_ID, role $ROLE, bucket $BUCKET"

    PROFILE=$(metadata instance/attributes/peppydb-profile)
    PROFILE=${PROFILE:-none}

    log "installing tools"
    if install_tools; then
        TOOLS=yes
    else
        TOOLS=no
        if [ "$PROFILE" != none ]; then
            finish failed "tool installation failed"
            return 1
        fi
        log "tool installation failed; continuing without profiling tools"
    fi

    if [ "$ROLE" = client ]; then
        PEPPYDB_BENCH_DIR=${PEPPYDB_BENCH_DIR:-$STATE/bench}
        mkdir -p "$PEPPYDB_BENCH_DIR"
    else
        log "setting up the Local SSD"
        setup_disk || {
            finish failed "Local SSD setup failed"
            return 1
        }
    fi
    export PEPPYDB_BENCH_DIR

    log "downloading artifacts"
    download_artifacts || {
        finish failed "artifact download or checksum failed"
        return 1
    }
    load_bench_env || {
        finish failed "bad bench.env"
        return 1
    }
    STEP_SECONDS=${PEPPYDB_BENCH_STEP_SECONDS:-$STEP_SECONDS}
    echo "$STEP_SECONDS" | grep -Eq '^[1-9][0-9]*$' || {
        finish failed "bad PEPPYDB_BENCH_STEP_SECONDS: $STEP_SECONDS"
        return 1
    }
    prepare_profiling || {
        finish failed "profiling setup failed"
        return 1
    }
    write_env
    upload "$RESULTS/env.txt"

    case $ROLE in
        single)
            run_suite
            case $? in
                0) ;;
                1)
                    finish failed "a preflight step failed"
                    return 1
                    ;;
                *)
                    finish failed "invalid suite or profiler list"
                    return 1
                    ;;
            esac
            ;;
        server | server-*)
            run_server_role || {
                finish failed "server role failed"
                return 1
            }
            ;;
        client)
            run_client_role || {
                finish failed "client role failed"
                return 1
            }
            ;;
    esac
    if awk '$3 != 0 { found = 1 } END { exit !found }' "$RESULTS/status"; then
        finish failed "some steps failed"
    else
        finish ok "all steps passed"
    fi
}

mkdir -p "$STATE"
main 2>&1 | tee -a "$LOG"
