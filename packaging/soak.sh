#!/bin/sh
# Memory soak for hearthd: cycles of opening terminals, flooding them with
# output and closing them, printing the server's resident memory, threads
# and open files at each step. A leak shows as numbers that keep rising
# from cycle to cycle.
#   packaging/soak.sh [TERMINALS] [MB_PER_TERMINAL] [CYCLES]   (default 50 20 3)
# Uses the release build and a private server; your own server is untouched.
set -eu
cd "$(dirname "$0")/.."
n=${1:-50} mb=${2:-20} cycles=${3:-3}
cargo build --release --locked -q
bin=$PWD/target/release
run=$(mktemp -d -p "${XDG_RUNTIME_DIR:-/tmp}" hsoak.XXXXXX)
state=$(mktemp -d)
export HEARTH_RUNTIME_DIR="$run" HEARTH_SOCKET="$run/s.sock" HEARTH_STATE_DIR="$state" HEARTH_CONFIG_DIR="$state/cfg"
"$bin/hearthd" --no-persist >"$state/hearthd.log" 2>&1 &
server=$!
trap 'kill $server 2>/dev/null; rm -rf "$run" "$state"' EXIT
while [ ! -S "$run/s.sock" ]; do sleep 0.05; done
h() { "$bin/hearth" "$@"; }
rss() {
    awk '/VmRSS/ {printf "%d MB", $2 / 1024}' "/proc/$server/status"
    printf ', %s threads, %s fds' "$(ls "/proc/$server/task" | wc -l)" "$(ls "/proc/$server/fd" | wc -l)"
}

echo "server pid $server; $n terminals x $mb MB, $cycles cycles"
echo "start: $(rss)"
for c in $(seq 1 "$cycles"); do
    t0=$(date +%s)
    for i in $(seq 1 "$n"); do
        h new -n "soak$i" -- sh -c "head -c $((mb * 1000000)) /dev/urandom | base64 -w 200; echo SOAK-\$((1+1))-DONE; sleep 3600" >/dev/null
    done
    for i in $(seq 1 "$n"); do
        b=$(h --json ls | python3 -c "import json,sys; print([b for s in json.load(sys.stdin) if s['name']=='soak$i' for b in s['blocks']][0])")
        h wait -t "$b" --text SOAK-2-DONE --timeout 600000 >/dev/null
    done
    echo "cycle $c: $n terminals flooded in $(( $(date +%s) - t0 ))s: $(rss)"
    for i in $(seq 1 "$n"); do h close-session "soak$i" >/dev/null; done
    sleep 5
    echo "cycle $c: all closed: $(rss)"
done
