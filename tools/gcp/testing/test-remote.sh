#!/bin/sh
# Offline tests of tools/gcp/remote-bench.sh (plan 0024). Linux only; run in
# the Linux container:
#
#   tools/linux-run sh tools/gcp/testing/test-remote.sh
#
# Fake test binaries print libtest-shaped output; stub gcloud, apt-get,
# systemctl, and snap commands record their calls. Nothing contacts GCP.
set -u
ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
SCRIPT=$ROOT/tools/gcp/remote-bench.sh
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
failures=0

fail() {
    echo "FAIL [$CASE]: $*" >&2
    failures=$((failures + 1))
}

# fake_binary NAME BEHAVIOUR: a fake test binary. pass: one test passes.
# skip: one test passes after printing a skip. fail: exits 101. empty: runs
# 0 tests. Every binary records its arguments and PEPPYDB_ settings.
fake_binary() {
    cat >"$CASE_DIR/stage/bin/$1" <<EOF
#!/bin/sh
echo "\$0 \$*" >>"$CASE_DIR/calls"
env | grep '^PEPPYDB_BENCH' | sort >>"$CASE_DIR/calls"
case $2 in
    empty) echo "running 0 tests"; echo; echo "test result: ok. 0 passed; 0 failed"; exit 0 ;;
    fail) echo "running 1 test"; echo "test x ... FAILED"; exit 101 ;;
    skip) echo "running 1 test"; echo "skipped: /tmp rejects O_DIRECT" >&2 ;;
    *) echo "running 1 test" ;;
esac
echo "test x ... ok"
echo
echo "test result: ok. 1 passed; 0 failed"
EOF
}

# new_case NAME: a fresh state directory, fake bucket, and stage directory
# with a two-step suite and passing binaries.
new_case() {
    CASE=$1
    CASE_DIR=$TMP/$CASE
    mkdir -p "$CASE_DIR/stage/bin" "$CASE_DIR/gcs" "$CASE_DIR/metadata" "$CASE_DIR/bench"
    fake_binary store pass
    fake_binary tree pass
    cat >"$CASE_DIR/stage/suite.txt" <<'EOF'
# test suite
preflight-store store --nocapture tests::direct::

bench-one tree --ignored --nocapture tests::bench::
bench-two tree --ignored tests::bench_io::
EOF
    printf 'PEPPYDB_BENCH_MILLIS=5\n' >"$CASE_DIR/stage/bench.env"
    echo test-bucket >"$CASE_DIR/metadata/instance_attributes_peppydb-bucket"
    echo run-1 >"$CASE_DIR/metadata/instance_attributes_peppydb-run-id"
    echo c4d-standard-8-lssd >"$CASE_DIR/metadata/instance_machine-type"
}

# publish: write MANIFEST and upload the stage directory to the fake bucket.
publish() {
    artifacts=$CASE_DIR/gcs/test-bucket/peppydb-bench/artifacts/run-1
    mkdir -p "$artifacts"
    (
        cd "$CASE_DIR/stage" || exit 1
        echo "run_id=run-1"
        echo "build_mode=release"
        find . -type f ! -name MANIFEST | sed 's|^\./||' | sort | while read -r path; do
            echo "sha256 $path $(sha256sum "$path" | cut -d' ' -f1)"
        done
    ) >"$CASE_DIR/stage/MANIFEST"
    cp -R "$CASE_DIR/stage/." "$artifacts/"
}

run_remote() {
    env PATH="$ROOT/tools/gcp/testing/bin:$PATH" \
        PEPPYDB_FAKE_LOG="$CASE_DIR/fake.log" \
        PEPPYDB_FAKE_GCS="$CASE_DIR/gcs" \
        PEPPYDB_REMOTE_STATE="$CASE_DIR/state" \
        PEPPYDB_REMOTE_METADATA="$CASE_DIR/metadata" \
        PEPPYDB_REMOTE_NO_MKFS=1 \
        PEPPYDB_BENCH_DIR="$CASE_DIR/bench" \
        "$@" sh "$SCRIPT" >"$CASE_DIR/output" 2>&1
}

results() {
    echo "$CASE_DIR/gcs/test-bucket/peppydb-bench/results/run-1"
}

expect_done() {
    done_file=$(results)/DONE
    if [ ! -f "$done_file" ]; then
        fail "no DONE"
        return
    fi
    grep -qx "status=$1" "$done_file" || fail "DONE is not status=$1: $(cat "$done_file")"
    [ -z "${2:-}" ] || grep -q "^reason=.*$2" "$done_file" ||
        fail "DONE reason is not '$2': $(cat "$done_file")"
}

expect_file() {
    [ -f "$(results)/$1" ] || fail "missing result file $1"
}

expect_no_file() {
    [ ! -f "$(results)/$1" ] || fail "unexpected result file $1"
}

expect_status() { # step profiler rc
    grep -q "^$1 $2 $3 " "$(results)/status" ||
        fail "status has no '$1 $2 $3': $(cat "$(results)/status")"
}

expect_log() {
    grep -q -- "$1" "$CASE_DIR/fake.log" || fail "fake log lacks '$1'"
}

# --- A passing run.
new_case ok
publish
run_remote
expect_done ok "all steps passed"
for file in started env.txt status startup-script.log preflight-store.log bench-one.log bench-two.log; do
    expect_file "$file"
done
expect_status preflight-store none 0
expect_status bench-one none 0
expect_status bench-two none 0
grep -q "store --nocapture tests::direct::" "$CASE_DIR/calls" || fail "preflight arguments"
grep -q "tree --ignored --nocapture tests::bench::" "$CASE_DIR/calls" || fail "bench arguments"
grep -q "^PEPPYDB_BENCH_MILLIS=5$" "$CASE_DIR/calls" || fail "bench.env not exported"
grep -q "^PEPPYDB_BENCH_DIR=$CASE_DIR/bench$" "$CASE_DIR/calls" || fail "PEPPYDB_BENCH_DIR not set"
grep -q "^machine_type=c4d-standard-8-lssd$" "$(results)/env.txt" || fail "env.txt machine type"
grep -q "^tools_installed=yes$" "$(results)/env.txt" || fail "env.txt tools_installed"
# Background jobs stop before apt-get, and perf matches the kernel.
first_stop=$(grep -n '^systemctl stop' "$CASE_DIR/fake.log" | head -n 1 | cut -d: -f1)
first_apt=$(grep -n '^apt-get' "$CASE_DIR/fake.log" | head -n 1 | cut -d: -f1)
[ -n "$first_stop" ] && [ -n "$first_apt" ] && [ "$first_stop" -lt "$first_apt" ] ||
    fail "background jobs not stopped before apt-get"
expect_log "unattended-upgrades"
expect_log "snap refresh --hold"
expect_log "linux-tools-$(uname -r)"
# started is uploaded before anything is installed.
first_upload=$(grep -n 'results/run-1/started' "$CASE_DIR/fake.log" | head -n 1 | cut -d: -f1)
[ -n "$first_upload" ] && [ "$first_upload" -lt "$first_apt" ] || fail "started not uploaded first"
# DONE is the last upload.
tail -n 1 "$CASE_DIR/fake.log" | grep -q 'results/run-1/DONE$' || fail "DONE is not the last upload"

# --- A second start of the same VM does nothing.
uploads_before=$(wc -l <"$CASE_DIR/fake.log")
run_remote
uploads_after=$(wc -l <"$CASE_DIR/fake.log")
[ "$uploads_before" = "$uploads_after" ] || fail "second start did work"
grep -q "already finished" "$CASE_DIR/output" || fail "second start did not report the guard"

# --- A reboot in the middle of a run reports the run as failed.
new_case reboot
publish
mkdir -p "$CASE_DIR/state/results"
echo earlier >"$CASE_DIR/state/started"
run_remote
expect_done failed "rebooted"
[ ! -f "$CASE_DIR/calls" ] || fail "a binary ran after a reboot"

# --- A failing benchmark step is recorded, and the run continues.
new_case step-fails
fake_binary tree fail
publish
run_remote
expect_done failed "some steps failed"
expect_status bench-one none 101
expect_status bench-two none 101
grep -q "failed_steps=bench-one:none,bench-two:none" "$(results)/DONE" || fail "failed_steps"

# --- A failing preflight step stops the run.
new_case preflight-fails
fake_binary store fail
publish
run_remote
expect_done failed "preflight"
expect_status preflight-store none 101
expect_no_file bench-one.log

# --- A preflight step that skips a test fails, though the test passed.
new_case preflight-skips
fake_binary store skip
publish
run_remote
expect_done failed "preflight"
expect_status preflight-store none 1
grep -q "a-test-was-skipped" "$(results)/status" || fail "skip not named in status"
expect_no_file bench-one.log

# --- A step whose filter matches no test fails.
new_case no-tests
fake_binary tree empty
publish
run_remote
expect_done failed "some steps failed"
grep -q "^bench-one none 1 .* no-tests-matched$" "$(results)/status" || fail "empty step not flagged"

# --- A corrupted binary stops the run at the checksum step.
new_case checksum
publish
echo corrupted >>"$CASE_DIR/gcs/test-bucket/peppydb-bench/artifacts/run-1/bin/tree"
run_remote
expect_done failed "checksum"
expect_no_file preflight-store.log
[ ! -f "$CASE_DIR/calls" ] || fail "a binary ran after a checksum failure"

# --- A failed tool installation does not stop an unprofiled run.
new_case apt-fails
publish
run_remote PEPPYDB_FAKE_APT_FAIL=1
expect_done ok
grep -q "^tools_installed=no$" "$(results)/env.txt" || fail "env.txt tools_installed=no"

# --- A failed tool installation stops a profiled run.
new_case apt-fails-profiled
publish
echo record >"$CASE_DIR/metadata/instance_attributes_peppydb-profile"
run_remote PEPPYDB_FAKE_APT_FAIL=1
expect_done failed "tool installation"
expect_no_file preflight-store.log

# --- bench.env may set only PEPPYDB_BENCH_* to plain values.
new_case bad-env
printf 'PATH=/evil\n' >"$CASE_DIR/stage/bench.env"
publish
run_remote
expect_done failed "bench.env"

# --- A suite line that names a binary that was not staged.
new_case bad-suite
echo "bench-three missing --ignored" >>"$CASE_DIR/stage/suite.txt"
publish
run_remote
expect_done failed "invalid suite"

# --- No metadata: nothing can be uploaded, but the script exits.
new_case no-metadata
rm "$CASE_DIR/metadata/instance_attributes_peppydb-bucket"
run_remote
grep -q "missing from metadata" "$CASE_DIR/output" || fail "missing metadata not reported"

if [ "$failures" -ne 0 ]; then
    echo "test-remote: $failures failure(s)" >&2
    exit 1
fi
echo "test-remote: all cases passed"
