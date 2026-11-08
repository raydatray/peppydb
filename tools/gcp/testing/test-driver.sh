#!/bin/sh
# Offline tests of tools/gcp-bench and tools/gcp/check-bucket.py (plan 0024).
# Linux only; run in the Linux container:
#
#   tools/linux-run sh tools/gcp/testing/test-driver.sh
#
# The stub gcloud in tools/gcp/testing/bin simulates GCS and Compute Engine.
# Its "VM" runs tools/gcp/remote-bench.sh in the background, so most cases
# are end to end: driver, stub gcloud, remote script, fake binaries. Nothing
# contacts GCP.
set -u
ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
DRIVER=$ROOT/tools/gcp-bench
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
failures=0

fail() {
    echo "FAIL [$CASE]: $*" >&2
    failures=$((failures + 1))
}

# A two-step suite with real binary names; the binaries are fakes.
mkdir -p "$TMP/prebuilt"
cat >"$TMP/suite.txt" <<'EOF'
# test suite
preflight-store bf-tree-store-unittest --nocapture tests::direct::
bench-wal       engine-wal-unittest    --ignored --nocapture tests::bench::
bench-engine    engine-unittest        --ignored --nocapture tests::bench::
EOF

fake_binary() { # name behaviour (pass or fail)
    cat >"$TMP/prebuilt/$1" <<EOF
#!/bin/sh
echo "$1 \$*" >>"$TMP/calls"
env | grep '^PEPPYDB_BENCH' | sort >>"$TMP/calls"
echo "running 1 test"
[ "$2" = fail ] && { echo "test x ... FAILED"; exit 101; }
echo "test x ... ok"
echo
echo "test result: ok. 1 passed; 0 failed"
EOF
    chmod +x "$TMP/prebuilt/$1"
}

bucket_json() { # location
    python3 -c '
import json, sys
rules = json.load(open(sys.argv[1]))["rule"]
# API-shaped: other key order, extra fields.
api = [{"condition": dict(reversed(list(r["condition"].items()))), "action": r["action"]} for r in rules]
print(json.dumps({"name": "test-bucket", "storageClass": "STANDARD", "location": sys.argv[2],
                  "lifecycle_config": {"rule": api}}))
' "$ROOT/tools/gcp/lifecycle.json" "$1"
}

new_case() {
    CASE=$1
    CASE_DIR=$TMP/$CASE
    mkdir -p "$CASE_DIR/gcs/test-bucket" "$CASE_DIR/compute"
    rm -f "$TMP/calls"
    fake_binary bf-tree-store-unittest pass
    fake_binary engine-wal-unittest pass
    fake_binary engine-unittest pass
    bucket_json US-EAST1 >"$CASE_DIR/bucket.json"
}

# exec_driver ARGS...: replace this (sub)shell with the driver, run against
# the stub gcloud.
exec_driver() {
    export PATH="$ROOT/tools/gcp/testing/bin:$PATH" \
        PEPPYDB_FAKE_LOG="$CASE_DIR/fake.log" \
        PEPPYDB_FAKE_GCS="$CASE_DIR/gcs" \
        PEPPYDB_FAKE_COMPUTE="$CASE_DIR/compute" \
        PEPPYDB_FAKE_BUCKET_JSON="$CASE_DIR/bucket.json" \
        PEPPYDB_GCP_BENCH_OUT="$CASE_DIR/out" \
        PEPPYDB_GCP_BENCH_TEST=1 \
        PEPPYDB_GCP_BENCH_POLL=1 \
        PEPPYDB_GCP_BENCH_BOOT_TIMEOUT=4 \
        PEPPYDB_GCP_BENCH_DEADLINE_MARGIN=1 \
        PEPPYDB_BENCH_MILLIS=7
    for setting in ${EXTRA_ENV:-}; do
        export "${setting?}"
    done
    exec sh "$DRIVER" "$@" >"$CASE_DIR/output" 2>&1
}

# driver ARGS...: run the driver. Sets CODE.
driver() {
    (exec_driver "$@")
    CODE=$?
}

run_args() {
    echo "run --project test-project --bucket test-bucket --suite $TMP/suite.txt --prebuilt $TMP/prebuilt --max-run 60s"
}

expect_code() {
    [ "$CODE" = "$1" ] || fail "exit code $CODE, expected $1; output: $(tail -n 5 "$CASE_DIR/output")"
}

expect_output() {
    grep -q -- "$1" "$CASE_DIR/output" || fail "output lacks '$1'"
}

expect_called() {
    grep -q -- "$1" "$CASE_DIR/fake.log" || fail "gcloud was not called with '$1'"
}

expect_not_called() {
    ! grep -q -- "$1" "$CASE_DIR/fake.log" 2>/dev/null || fail "gcloud was called with '$1'"
}

expect_no_instances() {
    remaining=$(ls "$CASE_DIR/compute/instances" 2>/dev/null | grep -v '\.describes$')
    [ -z "$remaining" ] || fail "instances remain: $remaining"
}

# run_dir: the local directory of the case's only run.
run_dir() {
    ls -d "$CASE_DIR"/out/* 2>/dev/null | head -n 1
}

# --- A passing run, end to end.
new_case ok
# shellcheck disable=SC2046
driver $(run_args)
expect_code 0
expect_output "run finished: all steps passed"
expect_called "compute instances create peppydb-bench-"
expect_called "--zone=us-east1-b"
expect_called "--provisioning-model=SPOT"
expect_called "--instance-termination-action=DELETE"
expect_called "--max-run-duration=60s"
expect_called "--image-family=ubuntu-2404-lts-amd64"
expect_called "--boot-disk-auto-delete"
expect_called "--service-account=peppydb-bench@test-project.iam.gserviceaccount.com"
expect_called "--labels=peppydb-bench=1,run="
expect_called "compute instances delete peppydb-bench-"
expect_not_called "--no-address"
expect_no_instances
grep -q "^PEPPYDB_BENCH_MILLIS=7$" "$TMP/calls" || fail "PEPPYDB_BENCH_MILLIS did not reach the VM"
grep -qsx "status=ok" "$(run_dir)/results/DONE" || fail "results not downloaded"
grep -qs "^sha256 bin/engine-wal-unittest " "$(run_dir)/artifacts/MANIFEST" ||
    fail "MANIFEST lacks the binary"
grep -qs "^PEPPYDB_BENCH_MILLIS=7$" "$(run_dir)/artifacts/bench.env" || fail "bench.env"
# The lifecycle and location check happens before any upload.
check_line=$(grep -n 'storage buckets describe' "$CASE_DIR/fake.log" | head -n 1 | cut -d: -f1)
upload_line=$(grep -n 'storage cp' "$CASE_DIR/fake.log" | head -n 1 | cut -d: -f1)
[ "$check_line" -lt "$upload_line" ] || fail "bucket check after upload"

# --- --step keeps the chosen step and the preflight steps.
new_case step
# shellcheck disable=SC2046
driver $(run_args) --step bench-engine
expect_code 0
grep -q "engine-unittest" "$TMP/calls" || fail "chosen step did not run"
grep -q "bf-tree-store-unittest" "$TMP/calls" || fail "preflight step did not run"
! grep -q "engine-wal-unittest" "$TMP/calls" || fail "unchosen step ran"

# --- An unknown step is refused before anything is uploaded.
new_case unknown-step
# shellcheck disable=SC2046
driver $(run_args) --step nope
expect_code 2
expect_not_called "storage cp"

# --- A failing benchmark step: the run completes, and the driver exits 1.
new_case step-fails
fake_binary engine-unittest fail
# shellcheck disable=SC2046
driver $(run_args)
expect_code 1
expect_output "bench-engine none 101"
expect_no_instances

# --- Capacity in the first zone: the next zone is used.
new_case capacity
EXTRA_ENV="PEPPYDB_FAKE_CAPACITY_ZONES=us-east1-b"
# shellcheck disable=SC2046
driver $(run_args)
EXTRA_ENV=
expect_code 0
expect_called "--zone=us-east1-c"
expect_no_instances

# --- No zone has capacity.
new_case no-capacity
EXTRA_ENV="PEPPYDB_FAKE_CAPACITY_ZONES=us-east1-b"
# shellcheck disable=SC2046
driver $(run_args) --zone us-east1-b
EXTRA_ENV=
expect_code 2
expect_output "no zone in"
expect_no_instances

# --- Another create error stops at once.
new_case create-error
EXTRA_ENV="PEPPYDB_FAKE_CREATE_ERROR=PERMISSION_DENIED"
# shellcheck disable=SC2046
driver $(run_args)
EXTRA_ENV=
expect_code 2
expect_output "not a capacity error"
expect_not_called "--zone=us-east1-c"

# --- A failed create that created the instance anyway.
new_case leak
EXTRA_ENV="PEPPYDB_FAKE_CAPACITY_ZONES=us-east1-b PEPPYDB_FAKE_CREATE_LEAKS=1"
# shellcheck disable=SC2046
driver $(run_args)
EXTRA_ENV=
expect_code 0
expect_output "left an instance behind"
expect_no_instances

# --- An instance without its time limit is deleted at once.
new_case no-limit
EXTRA_ENV="PEPPYDB_FAKE_NO_LIMIT=1 PEPPYDB_FAKE_VM=noboot"
# shellcheck disable=SC2046
driver $(run_args)
EXTRA_ENV=
expect_code 2
expect_output "lacks its time limit"
expect_no_instances

# --- The VM disappears (preempted) before DONE.
new_case vanish
EXTRA_ENV="PEPPYDB_FAKE_VM=vanish PEPPYDB_FAKE_PREEMPTED=1"
# shellcheck disable=SC2046
driver $(run_args)
EXTRA_ENV=
expect_code 3
expect_output "preempted"

# --- The VM stops before DONE.
new_case stop
EXTRA_ENV="PEPPYDB_FAKE_VM=stop"
# shellcheck disable=SC2046
driver $(run_args)
EXTRA_ENV=
expect_code 3
expect_output "stopped before the run finished"
expect_no_instances

# --- The VM never starts the run.
new_case no-boot
EXTRA_ENV="PEPPYDB_FAKE_VM=noboot"
# shellcheck disable=SC2046
driver $(run_args)
EXTRA_ENV=
expect_code 3
expect_output "did not start the run"
expect_called "get-serial-port-output"
expect_no_instances
[ -f "$(run_dir)/serial-port.txt" ] || fail "serial port output not saved"

# --- The run never finishes: the absolute deadline ends the wait.
new_case deadline
EXTRA_ENV="PEPPYDB_FAKE_VM=hang"
# shellcheck disable=SC2046
driver $(run_args) --max-run 3s
EXTRA_ENV=
expect_code 3
expect_output "did not finish within"
expect_no_instances

# --- A signal during the wait deletes the VM. INT and TERM share the exit
# path; the test sends TERM, because a non-interactive shell starts its
# background jobs with INT ignored, and an ignored signal cannot be trapped.
new_case interrupt
EXTRA_ENV="PEPPYDB_FAKE_VM=hang"
# shellcheck disable=SC2046
(exec_driver $(run_args) --max-run 600s) &
pid=$!
waited=0
until grep -qs "waiting for the run" "$CASE_DIR/output" || [ "$waited" -ge 30 ]; do
    sleep 1
    waited=$((waited + 1))
done
kill -TERM "$pid"
wait "$pid"
CODE=$?
EXTRA_ENV=
expect_code 143
expect_no_instances
expect_called "compute instances delete"

# --- --keep leaves the VM (its time limit still applies).
new_case keep
# shellcheck disable=SC2046
driver $(run_args) --keep
expect_code 0
expect_not_called "compute instances delete"
rm -rf "$CASE_DIR/compute/instances"

# --- The bucket must be in us-east1.
new_case wrong-region
bucket_json US-EAST4 >"$CASE_DIR/bucket.json"
# shellcheck disable=SC2046
driver $(run_args)
expect_code 2
expect_output "not US-EAST1"
expect_not_called "storage cp"

# --- Zones outside us-east1 are refused.
new_case zone-guard
# shellcheck disable=SC2046
driver $(run_args) --zone us-east4-a
expect_code 2
expect_output "outside us-east1"
[ ! -f "$CASE_DIR/fake.log" ] || fail "gcloud was called"

# --- cleanup deletes labelled instances and peppydb-bench disks.
new_case cleanup
mkdir -p "$CASE_DIR/compute/instances" "$CASE_DIR/compute/disks"
echo "us-east1-b TERMINATED --labels=peppydb-bench=1" >"$CASE_DIR/compute/instances/peppydb-bench-old"
echo us-east1-c >"$CASE_DIR/compute/disks/peppydb-bench-old-disk"
driver cleanup --project test-project
expect_code 0
expect_called "compute instances delete peppydb-bench-old --zone=us-east1-b"
expect_called "compute disks delete peppydb-bench-old-disk --zone=us-east1-c"
expect_no_instances

# --- --dry-run prints the sequence and runs no gcloud command.
new_case dry-run
# shellcheck disable=SC2046
driver $(run_args) --dry-run
expect_code 0
[ ! -f "$CASE_DIR/fake.log" ] || fail "gcloud was called in a dry run"
for expected in "+ gcloud storage buckets describe" "+ gcloud storage cp --quiet" \
    "+ gcloud compute instances create" "--max-run-duration=60s" \
    "+ gcloud compute instances describe" "+ gcloud storage ls" \
    "+ gcloud storage cp --recursive" "+ gcloud compute instances delete"; do
    expect_output "$expected"
done

# --- Without the test setting, a build on this host is refused unless it is
# x86_64 Ubuntu 24.04.
new_case host-check
if [ "$(uname -m)" != x86_64 ]; then
    env PATH="$ROOT/tools/gcp/testing/bin:$PATH" sh "$DRIVER" build \
        --suite "$TMP/suite.txt" --prebuilt "$TMP/prebuilt" >"$CASE_DIR/output" 2>&1
    CODE=$?
    expect_code 2
    expect_output "not Linux x86_64"
fi

# --- check-bucket.py compares rules, not text.
CASE=check-bucket
check() { # expected-exit json
    printf '%s' "$2" | python3 "$ROOT/tools/gcp/check-bucket.py" "$ROOT/tools/gcp/lifecycle.json" us-east1 \
        >"$TMP/check.out" 2>&1
    code=$?
    [ "$code" = "$1" ] || fail "exit $code, expected $1 for $2: $(cat "$TMP/check.out")"
}
A='{"action": {"type": "Delete"}, "condition": {"matchesPrefix": ["peppydb-bench/artifacts/"], "age": 7}}'
R='{"condition": {"age": 14, "matchesPrefix": ["peppydb-bench/results/"]}, "action": {"type": "Delete"}}'
check 0 "{\"location\": \"US-EAST1\", \"lifecycle_config\": {\"rule\": [$R, $A]}}"
check 0 "{\"location\": \"US-EAST1\", \"lifecycle\": {\"rule\": [$A, $R]}}"
check 1 "{\"location\": \"US-EAST1\", \"lifecycle_config\": {\"rule\": [$A]}}"
check 1 "{\"location\": \"US-EAST1\"}"
check 1 "{\"location\": \"US-EAST1\", \"lifecycle_config\": {\"rule\": [$A, $(echo "$R" | sed 's/14/13/')]}}"
check 1 "{\"location\": \"US-EAST1\", \"lifecycle_config\": {\"rule\": [$A, $R, {\"action\": {\"type\": \"Delete\"}, \"condition\": {\"age\": 10, \"matchesPrefix\": [\"peppydb-bench/\"]}}]}}"
check 1 "{\"location\": \"US-EAST1\", \"lifecycle_config\": {\"rule\": [$A, $R, {\"action\": {\"type\": \"Delete\"}, \"condition\": {\"age\": 3}}]}}"
check 0 "{\"location\": \"US-EAST1\", \"lifecycle_config\": {\"rule\": [$A, $R, {\"action\": {\"type\": \"Delete\"}, \"condition\": {\"age\": 30, \"matchesPrefix\": [\"peppydb-bench/\"]}}]}}"
check 0 "{\"location\": \"US-EAST1\", \"lifecycle_config\": {\"rule\": [$A, $R, {\"action\": {\"type\": \"Delete\"}, \"condition\": {\"age\": 1, \"matchesPrefix\": [\"other/\"]}}]}}"
grep -q "warning: unrelated rule" "$TMP/check.out" || fail "unrelated rule not warned about"
check 1 "{\"location\": \"US-EAST4\", \"lifecycle_config\": {\"rule\": [$A, $R]}}"

if [ "$failures" -ne 0 ]; then
    echo "test-driver: $failures failure(s)" >&2
    exit 1
fi
echo "test-driver: all cases passed"
