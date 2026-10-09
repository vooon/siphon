#!/usr/bin/env bash
# End-to-end test: rustfs bucket -> siphon backup -> PBS -> siphon restore ->
# second rustfs bucket, then compare bodies and metadata object by object.
#
# Needs: docker (or podman) with compose, aws CLI v2, jq, python3, and the siphon image
# (default `siphon:e2e`, override with SIPHON_IMAGE).
# KEEP=1 leaves the containers running afterwards.
set -euo pipefail
cd "$(dirname "$0")"

DOCKER=${DOCKER:-docker}
IMAGE=${SIPHON_IMAGE:-siphon:e2e}
WORK=$(mktemp -d)

export AWS_ACCESS_KEY_ID=e2e-access
export AWS_SECRET_ACCESS_KEY=e2e-secret-key
export AWS_DEFAULT_REGION=us-east-1
export AWS_ENDPOINT_URL=http://127.0.0.1:9000
export AWS_REQUEST_CHECKSUM_CALCULATION=when_required
export AWS_RESPONSE_CHECKSUM_VALIDATION=when_required

cleanup() {
    [[ -n ${HC_PID:-} ]] && kill "$HC_PID" 2>/dev/null
    rm -rf "$WORK"
    if [[ -z ${KEEP:-} ]]; then
        $DOCKER compose down -v >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

step() { echo "=== $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
pbs() { $DOCKER exec siphon-e2e-pbs "$@"; }
s3() { aws s3api "$@"; }

wait_for() {
    for _ in $(seq 1 60); do
        curl -sk -o /dev/null "$1" && return 0
        sleep 2
    done
    fail "$1 did not come up"
}

step "start healthchecks stand-in"
python3 -I hc-mock.py 18080 "$WORK/hc.jsonl" &
HC_PID=$!
export HC_PING_URL=http://127.0.0.1:18080/ping/e2e

step "start rustfs and PBS"
$DOCKER compose up -d
wait_for http://127.0.0.1:9000/
wait_for https://127.0.0.1:8007/

step "bootstrap PBS: datastore, API token, ACL"
pbs sh -c 'mkdir -p /datastore/e2e && chown backup:backup /datastore/e2e'
pbs proxmox-backup-manager datastore create e2e /datastore/e2e >/dev/null
pbs proxmox-backup-manager user create siphon@pbs
PBS_PASSWORD=$(pbs proxmox-backup-manager user generate-token siphon@pbs e2e |
    sed 's/^Result: //' | jq -r .value)
for id in 'siphon@pbs' 'siphon@pbs!e2e'; do
    pbs proxmox-backup-manager acl update /datastore/e2e DatastoreBackup --auth-id "$id"
done
PBS_FINGERPRINT=$(pbs proxmox-backup-manager cert info | sed -n 's/^Fingerprint (sha256): //p')
export PBS_PASSWORD PBS_FINGERPRINT

siphon() {
    $DOCKER run --rm --network host \
        -e PBS_REPOSITORY='siphon@pbs!e2e@127.0.0.1:e2e' \
        -e PBS_PASSWORD -e PBS_FINGERPRINT \
        -e S3_ENDPOINT="$AWS_ENDPOINT_URL" -e S3_PATH_STYLE=true \
        -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY -e HC_PING_URL \
        -e SIPHON_PART_SIZE=$((5 << 20)) \
        "$IMAGE" "$@"
}

step "seed bucket 'source'"
s3 create-bucket --bucket source >/dev/null
s3 create-bucket --bucket restored >/dev/null
head -c $((1 << 20)) /dev/urandom >"$WORK/wal"
head -c $((12 << 20)) /dev/urandom >"$WORK/base"
: >"$WORK/empty"
echo hello >"$WORK/small"
put() { s3 put-object --bucket source --key "$1" --body "$2" "${@:3}" >/dev/null; }
put wal/000000010000000000000001 "$WORK/wal" \
    --content-type application/octet-stream --metadata barman=wal,timeline=1
# multipart upload on the source side (ETag "<md5>-<parts>")
aws s3 cp --quiet "$WORK/base" s3://source/base/20261008T031500/data.tar \
    --content-type application/x-tar --cache-control no-cache --metadata kind=base
put empty "$WORK/empty"
# rustfs rejects keys like "a//b", "./x", "a/../b"; the escaping of those is
# covered by unit tests (src/tree.rs).
put "dir/" "$WORK/empty"                           # folder marker
put a "$WORK/small"                                # file and ...
put a/b "$WORK/small"                              # ... directory with the same name
put "/lead" "$WORK/small" --content-language de    # stored as "lead" by rustfs
put "ünïcödé/fïlé with space.txt" "$WORK/small" \
    --content-type "text/plain; charset=utf-8" --content-disposition attachment

step "siphon backup: first, unchanged, one new object (reuse 0%, 100%, partial)"
siphon backup --s3-bucket source 2>&1 | tee "$WORK/backup1.log"
sleep 2 # snapshot times have 1 s resolution
siphon backup --s3-bucket source 2>&1 | tee "$WORK/backup2.log"
grep -q 'reused 0 B (0.0%)' "$WORK/backup1.log" || fail "first backup reported reuse"
# nothing changed: the second run must reuse every chunk of the archive
grep -q 'reused .* (100.0%)' "$WORK/backup2.log" || fail "second backup did not reuse all chunks"
# a new WAL segment arrives: most of the archive is reused, not all of it
head -c $((1 << 20)) /dev/urandom >"$WORK/wal2"
put wal/000000010000000000000002 "$WORK/wal2" \
    --content-type application/octet-stream --metadata barman=wal,timeline=1
sleep 1
siphon backup --s3-bucket source 2>&1 | tee "$WORK/backup3.log"
pct=$(sed -n 's/.*reused .* (\([0-9.]*\)%).*/\1/p' "$WORK/backup3.log")
awk -v p="$pct" 'BEGIN { exit !(p > 50 && p < 100) }' || fail "third backup reused $pct%"

step "PBS verify"
pbs proxmox-backup-manager verify e2e | tail -1 | grep -q 'TASK OK' || fail "verify"

step "PBS web UI: catalog browsing and single-file download"
api() {
    curl -skf -H "Authorization: PBSAPIToken=siphon@pbs!e2e:$PBS_PASSWORD" \
        "https://127.0.0.1:8007/api2/json/admin/datastore/e2e/$1"
}
b64() { printf %s "$1" | base64 -w0; }
time=$(api snapshots | jq '.data | max_by(."backup-time") | ."backup-time"')
snap="backup-type=host&backup-id=source&backup-time=$time"
api "catalog?$snap&filepath=$(b64 /source.pxar.didx/ünïcödé)" |
    jq -e '.data == [{"text": "fïlé with space.txt", "type": "f", "size": 6}] or
        (.data | length == 1 and .[0].size == 6)' >/dev/null || fail "catalog listing"
api "pxar-file-download?$snap&filepath=$(b64 /source.pxar.didx/wal/000000010000000000000001)" \
    >"$WORK/download"
cmp -s "$WORK/wal" "$WORK/download" || fail "single-file download differs"

step "siphon restore --dry-run lists every object"
expected=$(s3 list-objects-v2 --bucket source | jq '.Contents | length')
listed=$(siphon restore --s3-bucket restored --snapshot host/source --dry-run | wc -l)
[[ $listed -eq $expected ]] || fail "dry run listed $listed of $expected objects"

step "siphon restore into 'restored'"
siphon restore --s3-bucket restored --snapshot host/source

step "restore refuses a non-empty bucket without --overwrite"
if siphon restore --s3-bucket restored --snapshot host/source 2>/dev/null; then
    fail "restore into non-empty bucket succeeded"
fi

step "compare objects"
keys() { s3 list-objects-v2 --bucket "$1" | jq -r '.Contents[].Key' | sort; }
diff <(keys source) <(keys restored) || fail "key lists differ"
meta() {
    s3 head-object --bucket "$1" --key "$2" | jq -S '{ContentLength, ContentType,
        CacheControl, ContentEncoding, ContentDisposition, ContentLanguage, Metadata}'
}
n=0
while IFS= read -r key; do
    s3 get-object --bucket source --key "$key" "$WORK/src" >/dev/null
    s3 get-object --bucket restored --key "$key" "$WORK/dst" >/dev/null
    cmp -s "$WORK/src" "$WORK/dst" || fail "body differs: $key"
    diff <(meta source "$key") <(meta restored "$key") || fail "metadata differs: $key"
    n=$((n + 1))
done < <(keys source)

step "healthchecks pings"
# 6 runs: 3 backups, dry run, restore, refused restore
hc() { jq -s "$1" "$WORK/hc.jsonl"; }
[[ $(hc '[.[] | select(.path == "/ping/e2e/start")] | length') -eq 6 ]] || fail "start pings"
# every start is paired with exactly one success or fail ping of the same run ID
[[ $(hc 'group_by(.rid) | map(length == 2 and .[0].rid != "") | all') == true ]] ||
    fail "unpaired pings: $(cat "$WORK/hc.jsonl")"
hc '[.[] | select(.path == "/ping/e2e")] | map(.body) | any(test("^backup host/source/.* done: 8 files"))' |
    grep -q true || fail "no backup success ping"
hc '[.[] | select(.path == "/ping/e2e/fail")] | map(.body) | any(test("not empty"))' |
    grep -q true || fail "no fail ping for refused restore"

echo "PASS: $n objects round-tripped, healthchecks pinged"
