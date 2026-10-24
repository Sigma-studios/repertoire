set -u
S=$HOME/target/debug/sentinelle
R=$HOME/target/release
sudo pkill repertoired; sleep 1; sudo rm -f /var/tmp/bench/big
echo "=== 1. daemon stopped"
XDG_CACHE_HOME=/tmp/sent-cache-1 $S --dump 2>&1 | grep -A4 "^disk"
echo "=== 2. daemon running"
sudo rm -rf /var/lib/repertoire
sudo $R/repertoired > /tmp/rd.log 2>&1 &
for i in $(seq 60); do $R/repertoire status 2>/dev/null | grep -q "^  live" && break; sleep 0.5; done
$R/repertoire status | head -6
(sleep 8; echo ">>> writing 3 GiB to /var/tmp/bench/big at $(date +%T)"; head -c 3G /dev/zero > /var/tmp/bench/big; sync; echo ">>> done at $(date +%T)") & WRITER=$!
XDG_CACHE_HOME=/tmp/sent-cache-2 SENTINELLE_DUMP_FOR=25 $S --dump 2>&1 | grep --line-buffered -v "^\(cpu\|gpu\|net\|host\|memory\|app\)" | awk '{print strftime("%T"), $0; fflush()}'
wait $WRITER
echo "=== 3. daemon stopped again"
sudo pkill repertoired; sleep 1
XDG_CACHE_HOME=/tmp/sent-cache-3 $S --dump 2>&1 | grep -A2 "^disk"
rm -f /var/tmp/bench/big
