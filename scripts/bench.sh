#!/usr/bin/env bash
# Benchmarks for répertoire, run inside the Linux VM as root:
#   scripts/vm.sh run 'cargo build --release && sudo scripts/bench.sh [scan|daemon|build|all]'
# Results are printed as they come; nothing is uploaded anywhere.
set -euo pipefail

BIN=${BIN:-$HOME/target/release}
[ -x "$BIN/repertoire" ] || BIN=$(dirname "$(readlink -f "$0")")/../target/release
B=/var/tmp/bench
SYN=$B/home520k
mkdir -p $B

cpu_ticks() { awk '{print $14+$15}' /proc/$1/stat; }
hwm() { awk '/VmHWM/{print $2" "$3}' /proc/$1/status; }
rss() { awk '/VmRSS/{print $2" "$3}' /proc/$1/status; }

synthetic() {
    [ -d $SYN ] && return
    echo "== generating ~520k files in $SYN"
    python3 - "$SYN" <<'PY'
import os, random, sys
root = sys.argv[1]; random.seed(1)
n = 0
for a in range(40):
    for b in range(40):
        d = os.path.join(root, f"proj{a:02d}", f"dir{b:02d}", random.choice(["src", "target/debug/deps", ".cache", "assets"]))
        os.makedirs(d, exist_ok=True)
        for f in range(325):
            # Mostly small real files; big ones sparse so the VM disk copes.
            size = random.choice([0, 100, 1000, 4000, 12000]) if f % 50 else 50_000_000
            with open(os.path.join(d, f"file_{f}_{random.randrange(1<<30):x}.rs"), "wb") as fh:
                fh.truncate(size) if size > 12000 else fh.write(b"x" * size)
            n += 1
print(n, "files")
PY
}

scan() {
    echo "== M1: repertoire scan / vs du -sx /"
    sync; "$BIN/repertoire" scan / --top 5 >/dev/null  # warm the cache
    /usr/bin/time -f "scan: %e s wall, %M KiB max RSS" "$BIN/repertoire" scan / --top 20
    local ours du
    ours=$("$BIN/repertoire" scan / --top 0)
    ours=${ours%%$'\n'*}
    du=$(du -sx --block-size=1 / 2>/dev/null | cut -f1)
    echo "ours: $ours"
    echo "du -sx: $du bytes ($((du >> 20)) MiB)"
    local mine; mine=$(echo "$ours" | sed -n 's/.*(\([0-9]*\) bytes).*/\1/p')
    python3 -c "print(f'difference: {100*abs($mine-$du)/$du:.3f}%')"
    synthetic
    echo "== scan of the synthetic tree (fast mode: all cores)"
    "$BIN/repertoire" scan $SYN --top 0 >/dev/null
    /usr/bin/time -f "scan: %e s wall, %M KiB max RSS" "$BIN/repertoire" scan $SYN --top 3
    sync; echo 3 > /proc/sys/vm/drop_caches
    /usr/bin/time -f "cold-cache scan: %e s wall" "$BIN/repertoire" scan $SYN --top 0
    du -s --block-size=1 $SYN
}

start_daemon() {  # args: extra daemon options
    rm -f $B/s.sock
    "$BIN/repertoired" --socket $B/s.sock --state-dir $B/state "$@" 2>$B/daemon.log &
    DPID=$!
    for _ in $(seq 600); do
        if "$BIN/repertoire" --socket $B/s.sock status 2>/dev/null | grep -q "live\|not watched"; then
            if ! "$BIN/repertoire" --socket $B/s.sock status | grep -q "scanning\|reconciling"; then return; fi
        fi
        sleep 0.5
    done
    echo "daemon never got live"; cat $B/daemon.log; exit 1
}

daemon() {
    synthetic
    echo "== daemon on the synthetic tree"
    rm -rf $B/state
    start_daemon --root $SYN
    grep scanned $B/daemon.log
    echo "RSS after first scan: $(rss $DPID) (peak $(hwm $DPID))"
    "$BIN/repertoire" --socket $B/s.sock status | sed -n '3,6p'
    echo "-- search latency (client side, includes connect + JSON):"
    for q in file_1 proj07 target deps a3f zzzz_nothing; do
        "$BIN/repertoire" --socket $B/s.sock search "$q" --limit 50 2>&1 >/dev/null | sed "s/^/$q: /"
    done
    echo "-- the same through the Albert plugin's client code:"
    PLUGIN_DIR=$(dirname "$(readlink -f "$0")")/../albert python3 - <<'PY'
import os, sys, time, types
sys.modules["albert"] = types.ModuleType("albert")
for n in ["PluginInstance", "GeneratorQueryHandler"]:
    setattr(sys.modules["albert"], n, type(n, (), {}))
sys.modules["albert"].__all__ = ["PluginInstance", "GeneratorQueryHandler"]
sys.path.insert(0, os.environ["PLUGIN_DIR"])
import repertoire as p
p.SOCKET = "/var/tmp/bench/s.sock"
for q in ["file_1", "proj07", "deps"]:
    t = time.perf_counter(); hits = p.search(q); dt = time.perf_counter() - t
    print(f"{q}: {len(hits)} hits in {dt*1000:.1f} ms; best {hits[0][0] if hits else None}")
PY
    echo "-- restart on the snapshot"
    kill -TERM $DPID; wait $DPID || true
    ls -la $B/state
    start_daemon --root $SYN
    grep -E "loaded snapshot|scanned" $B/daemon.log
    echo "RSS: $(rss $DPID) (peak $(hwm $DPID))"
    kill -TERM $DPID; wait $DPID || true
}

build() {
    echo "== M3: cargo build of Bevy under the daemon (indexing every real filesystem)"
    rm -rf $B/state
    start_daemon ${BUILD_DAEMON_ARGS:-}
    grep scanned $B/daemon.log
    local t0 c0
    c0=$(cpu_ticks $DPID); t0=$(date +%s.%N)
    echo "daemon before build: CPU $c0 ticks, RSS $(rss $DPID), peak $(hwm $DPID)"
    (cd $B/bevy && rm -rf target && sudo -u "${SUDO_USER:-root}" env PATH="$PATH" CARGO_HOME="$CARGO_HOME" RUSTUP_HOME="$RUSTUP_HOME" \
        cargo build --quiet 2>&1 | tail -3)
    local t1 c1 hz
    t1=$(date +%s.%N); c1=$(cpu_ticks $DPID); hz=$(getconf CLK_TCK)
    sleep 3
    local c2; c2=$(cpu_ticks $DPID)
    python3 -c "
wall=$t1-$t0; cpu=($c1-$c0)/$hz; settle=($c2-$c1)/$hz
print(f'build wall {wall:.0f} s; daemon CPU during build {cpu:.2f} s = {100*cpu/wall:.2f}% of one core; +{settle:.2f} s settling')"
    echo "daemon after build: RSS $(rss $DPID), peak $(hwm $DPID)"
    grep -c . $B/daemon.log >/dev/null
    echo "-- target/ according to the daemon vs du:"
    "$BIN/repertoire" --socket $B/s.sock ls $B/bevy | grep -E "target/|^[0-9. ]+[KMG]iB  /"
    du -s --block-size=1 $B/bevy/target $B/bevy
    kill -TERM $DPID; wait $DPID || true
    grep -E "kernel events" $B/daemon.log | tail -15 || true
    tail -3 $B/daemon.log
}

case "${1:-all}" in
scan) scan ;;
daemon) daemon ;;
build) build ;;
all) scan; daemon; build ;;
esac
