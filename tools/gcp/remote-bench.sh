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
PACKAGES="linux-tools-common bpftrace sysstat nvme-cli zstd"

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
    upload "$RESULTS/env.txt" "$RESULTS/status" "$LOG"
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

# Profiler hooks. Plan 0024 diff 5 adds the profilers; until then only
# "none" exists.
env_extra() { :; }
prepare_profiling() { :; }
known_profiler() { [ "$1" = none ]; }

# run_profiled PROFILER NAME BIN ARGS...: run one step under one profiler,
# with stdout and stderr in RESULTS/NAME.log or RESULTS/NAME.PROFILER.log.
# Prints the files to upload, one per line, on fd 3.
run_profiled() {
    profiler=$1 name=$2 bin=$3
    shift 3
    "$ARTIFACTS/bin/$bin" "$@" >"$RESULTS/$name.log" 2>&1
    rc=$?
    echo "$RESULTS/$name.log" >&3
    return "$rc"
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
# one test. A preflight step fails if a test reports a skip.
check_output() {
    if grep -q '^running 0 tests' "$2"; then
        echo "no tests matched"
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
            # shellcheck disable=SC2086 # args split on whitespace by design
            files=$(run_profiled "$profiler" "$name" "$bin" $args 3>&1 </dev/null)
            rc=$?
            elapsed=$(($(date +%s) - started))
            note=ok
            [ "$rc" = 0 ] || note=exit-$rc
            if problem=$(check_output "$name" "$(step_log "$name" "$profiler")"); then
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

main() {
    BUCKET=$(metadata instance/attributes/peppydb-bucket)
    RUN_ID=$(metadata instance/attributes/peppydb-run-id)
    if [ -z "$BUCKET" ] || [ -z "$RUN_ID" ]; then
        log "peppydb-bucket or peppydb-run-id missing from metadata"
        return 1
    fi
    REMOTE_ARTIFACTS=gs://$BUCKET/peppydb-bench/artifacts/$RUN_ID
    REMOTE_RESULTS=gs://$BUCKET/peppydb-bench/results/$RUN_ID

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
    log "run $RUN_ID, bucket $BUCKET"

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

    log "setting up the Local SSD"
    setup_disk || {
        finish failed "Local SSD setup failed"
        return 1
    }
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
    prepare_profiling || {
        finish failed "profiling setup failed"
        return 1
    }
    write_env
    upload "$RESULTS/env.txt"

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
    if awk '$3 != 0 { found = 1 } END { exit !found }' "$RESULTS/status"; then
        finish failed "some steps failed"
    else
        finish ok "all steps passed"
    fi
}

mkdir -p "$STATE"
main 2>&1 | tee -a "$LOG"
