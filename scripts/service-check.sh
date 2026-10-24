#!/usr/bin/env bash
# Install examples/repertoire.service in the VM and check the sandboxed
# daemon still watches: start it, write a file, see it counted.
#   scripts/vm.sh run 'cargo build --release && sudo scripts/service-check.sh'
set -euo pipefail
BIN=${BIN:-$HOME/target/release}
install -m 755 "$BIN/repertoired" "$BIN/repertoire" /usr/local/bin/
install -m 644 "$(dirname "$0")/../examples/repertoire.service" /etc/systemd/system/
systemctl daemon-reload
systemctl restart repertoire
for _ in $(seq 120); do repertoire status 2>/dev/null | grep -q "^  live" && break; sleep 0.5; done
repertoire status
journalctl -u repertoire --no-pager -n 8 -o cat
home=$(getent passwd "$SUDO_USER" | cut -d: -f6)
d=$home/repertoire-service-check
rm -rf "$d"; sudo -u "$SUDO_USER" mkdir -p "$d"
sudo -u "$SUDO_USER" head -c 50M /dev/urandom > "$d/blob"
sleep 2
echo "-- after writing 50 MiB in $d:"
repertoire ls "$d" -a
repertoire search repertoire-service-check
echo "-- as $SUDO_USER (filtered):"
sudo -u "$SUDO_USER" repertoire top "$home" -n 3
rm -rf "$d"
systemctl show repertoire -p MainPID -p MemoryCurrent -p CPUUsageNSec
systemctl stop repertoire
journalctl -u repertoire --no-pager -n 3 -o cat
ls -la /var/lib/repertoire/
