# répertoire

A system-wide index of **where disk space goes and what files are called**,
kept current by the kernel instead of rescans.

`repertoired` scans every real filesystem once, then follows every change
through fanotify, so folder sizes are always current and any file can be
found by name in milliseconds. Programs ask it over a local socket:

- **sentinelle**'s Storage view shows live numbers instead of rescanning
  every 20 minutes;
- the **Albert plugin** finds files by name (`f <name>`);
- the **`repertoire` CLI**: `status`, `top`, `ls`, `search`, `tree`, `watch`;
- later, a file explorer (the protocol already has what it needs).

The live part is Linux only (fanotify, kernel 5.17+). The scanner, the index
and the CLI also build and run on macOS, where the daemon rescans instead.

## Build

```sh
cargo build --release
# target/release/repertoired  (daemon)
# target/release/repertoire   (CLI)
```

Crates:

| crate | what |
|---|---|
| `repertoire-core` | Scanner, in-memory tree, applying change events, queries, access control, snapshots. Platform-neutral. |
| `repertoire-linux` | fanotify, file handles, mount table, I/O priority. Empty elsewhere. |
| `repertoire-proto` | Wire types and a blocking client (`serde`, `serde_json` only). What other programs depend on. |
| `repertoire` | The daemon (as a library and `repertoired`) and the CLI. |

## Run

As a service: see [`examples/repertoire.service`](examples/repertoire.service).
By hand:

```sh
sudo repertoired --verbose          # every real filesystem, live
repertoired --root ~/Projects --no-state --socket /tmp/r.sock   # one folder, scan only
repertoire status
repertoire top ~ -n 20
repertoire ls ~/.cache -a --sort size
repertoire search steam --kind dir
repertoire tree / --min 10G
repertoire watch ~ /                 # sizes as they change
repertoire scan ~/Projects           # no daemon: index here and now
```

Daemon options: `--root DIR` (repeatable; default: every real filesystem
from the mount table, the same list as sentinelle's: ext4, btrfs, xfs, …,
2 GiB or more, without snaps, container storage and `/run`), `--exclude
PATH`, `--exclude-name NAME` (none by default: the point is to see
everything), `--state-dir DIR` / `--no-state`, `--socket PATH`, `--no-watch`,
`--verbose`.

## How it works

- **Scan.** A parallel walk (rayon) with `openat`/`fstatat` relative to each
  directory, never crossing into another mount. Sizes are allocated bytes
  (`st_blocks × 512`), so sparse files count what they really use; hard
  links count once. The first scan uses every core; later ones run on two
  threads at `nice 19` and idle I/O priority. Each directory's file handle
  is recorded on the way.
- **Index.** One arena per filesystem: about 100–140 bytes per entry
  (names included). Every change is applied as a delta up the parent chain,
  so totals never need recomputing.
- **Events.** One fanotify group (`FAN_REPORT_DFID_NAME_TARGET`), one
  filesystem mark per indexed filesystem. Events name the parent directory by
  file handle plus the entry name. Applying an event means stat'ing that
  entry again and making the tree agree, which makes late, merged or
  reordered events harmless. Renames move subtrees. Content changes are
  debounced (applied after 1 s of quiet, at least every 5 s), and events are
  read every 100 ms, so the kernel merges a build's thousands of writes per
  file into one event.
- **Overflow.** If events are ever lost, the filesystem is flagged `stale`,
  keeps being served, and is rescanned at idle priority.
- **Snapshots.** Every 5 minutes and on shutdown, in
  `/var/lib/repertoire/<fs>.snap` (versioned, checksummed, written
  atomically; a bad one is discarded). On start the snapshot is served at
  once (`reconciling`) while a scan checks it; events seen during that scan
  are applied again to its result, so nothing slips between scan and watch.

## Socket and security model

`/run/repertoire/repertoire.sock`, newline-delimited JSON, documented in
[`docs/PROTOCOL.md`](docs/PROTOCOL.md).

The daemon runs as root (fanotify filesystem marks need `CAP_SYS_ADMIN`,
file handles `CAP_DAC_READ_SEARCH`, and it must read every folder) and keeps
only those two capabilities in the example unit, with the filesystem
read-only except its state directory and no network. The socket is
world-writable, but every answer is filtered for the peer's uid and groups
(read from the kernel): a client only sees entries it could list itself.
Names inside another user's `0700` home never leak; folder totals do (they
include what's inside unreadable subfolders, much like `df`). ACLs are
ignored in this version.

## Limits

- Linux 5.17+ for live updates (`FAN_RENAME`, target FIDs); 6.0+ for the
  kernel to ignore our own snapshot writes.
- A file hard-linked after it was indexed may count twice for up to 10 s.
- New mounts and unmounts are noticed within 30 s.
- Nested btrfs subvolumes that aren't mounted are skipped (as sentinelle
  does) and show up as space that can't be attributed.
- No full-text indexing: names and sizes only.
- On macOS (and without root) the daemon scans and rescans; no watching.

## Development

```sh
cargo test --workspace                             # macOS or Linux
cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu
python3 -m unittest discover -s albert/tests       # Albert plugin
scripts/vm.sh create                               # Lima Ubuntu 24.04 VM (once)
scripts/vm.sh test                                 # tests in the VM, then as root
scripts/vm.sh run 'cargo build --release && sudo env BIN=$HOME/target/release scripts/bench.sh'
```

The randomized tests (`crates/repertoire-core/tests/events.rs`,
`crates/repertoire/tests/fanotify.rs`) take `REPERTOIRE_SEED`,
`REPERTOIRE_OPS` and, for the first, `REPERTOIRE_CHECK_EVERY` and
`REPERTOIRE_TRACE`.

## Albert plugin

[`albert/repertoire`](albert/repertoire): copy (or link) the folder to
`~/.local/share/albert/python/plugins/repertoire`, enable *Répertoire* in
Albert's settings, then type `f ` and a name. Enter opens; the other actions
open the containing folder or copy the path. Needs Albert's Python
interface 5.0 (Albert v35+).

Queries with a slash are paths:

- `f bevy_rues/t`: what's inside folders named `bevy_rues` (anywhere) that
  matches `t`. Every segment before the last slash is an **exact** folder
  name (any case), each one directly inside the previous one; only the last
  segment matches loosely, ranked like a normal search.
  `f bevy_rues/` lists the folder, largest first.
- `f ~/Projects/bev` or `f /etc/sys`: a real path, listed from the
  filesystem (folder totals from the daemon when it runs).
- Tab on a folder completes it with a slash, to go one level down.

A path query is one search for the first folder and then one listing per
level, on a single connection: a few milliseconds. For very common first
names (`src/…`) only the 20 largest such folders are followed; start the
path one level higher to be specific.
