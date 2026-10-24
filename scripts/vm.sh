#!/usr/bin/env bash
# Linux test VM for répertoire (Lima, Ubuntu 24.04, native arch).
#
#   scripts/vm.sh create     create and provision the VM (once)
#   scripts/vm.sh test [ARGS] cargo test as a user, then again under sudo
#                             (root-only tests skip themselves otherwise)
#   scripts/vm.sh run CMD..   run a command in the repo inside the VM
#   scripts/vm.sh shell       interactive shell in the repo
#   scripts/vm.sh delete      remove the VM
#
# The repo is mounted at the same path; build output goes to
# ~/target inside the VM so it never mixes with the macOS one. Tests that
# need a real ext4 directory use /var/tmp (the VM's root filesystem).
set -euo pipefail

VM=${REPERTOIRE_VM:-repertoire}
REPO=$(cd "$(dirname "$0")/.." && pwd)
# Sibling checkouts mounted too, so sentinelle can be built against this repo.
EXTRA_MOUNT=$(cd "$REPO/.." && pwd)

in_vm() {
    limactl shell --workdir "$REPO" "$VM" bash -lc "$*"
}

case "${1:-}" in
create)
    limactl create --name "$VM" --tty=false \
        --vm-type vz --mount-type virtiofs \
        --cpus "${VM_CPUS:-6}" --memory "${VM_MEMORY:-8}" --disk "${VM_DISK:-40}" \
        --set ".mounts = [{\"location\": \"$EXTRA_MOUNT\", \"writable\": true}]" \
        template://ubuntu-24.04
    limactl start "$VM"
    in_vm 'sudo apt-get update -qq && sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq build-essential pkg-config git time >/dev/null'
    in_vm 'command -v cargo >/dev/null || curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --component clippy'
    in_vm 'uname -r'
    ;;
test)
    shift
    in_vm "export CARGO_TARGET_DIR=\$HOME/target; cargo test --workspace $* && \
        sudo env PATH=\$PATH HOME=\$HOME RUSTUP_HOME=\$HOME/.rustup CARGO_HOME=\$HOME/.cargo \
            CARGO_TARGET_DIR=\$HOME/target-root cargo test --workspace $*"
    ;;
run)
    shift
    in_vm "export CARGO_TARGET_DIR=\$HOME/target; $*"
    ;;
shell)
    limactl shell --workdir "$REPO" "$VM"
    ;;
delete)
    limactl delete --force "$VM"
    ;;
*)
    sed -n '2,13p' "$0"
    exit 2
    ;;
esac
