# répertoire protocol, version 1

The daemon listens on a Unix stream socket, `/run/repertoire/repertoire.sock`
by default (`REPERTOIRE_SOCKET` overrides it for clients, `--socket` for the
daemon). The socket is mode `0666`; every answer is filtered for the uid on
the other end (see [Access control](#access-control)).

Messages are **newline-delimited JSON**: one JSON object per line, UTF-8,
no line breaks inside. Clients send one request per line and read one
response per line, in order. A connection can carry any number of requests.
`Subscribe` is the exception: it turns the connection into an event stream
until either side closes it.

`repertoire-proto` implements all of this in Rust (types and a blocking
client); `albert/repertoire/__init__.py` shows a 40-line Python client.

## Common fields

Every message, both ways, has:

| field  | type   | meaning |
|--------|--------|---------|
| `v`    | number | Protocol version. This document describes `1`. |
| `type` | string | The message type, below. |

A request with another `v` gets an `Error` with code `version`. New
optional fields may be added to answers within a version; clients must
ignore fields they don't know.

### Paths and names

Unix paths are bytes. A path travels in a `path` field as a string when it's
valid UTF-8, and otherwise in a `path_b64` field as standard base64 (with
padding) of the raw bytes. The same goes for `name`/`name_b64` (entry names)
and `under`/`under_b64` (search scope). Exactly one of the pair is present.

```json
{"path": "/home/sigma/Été 2024"}
{"path_b64": "L2hvbWUvc2lnbWEvY2Fm6S50eHQ="}
```

(The second is `/home/sigma/caf\xe9.txt`, a Latin-1 name.)

Paths in requests must be absolute; `..` is rejected. Trailing slashes
don't matter.

### Sizes

All sizes are bytes, as integers.

- **Allocated** size (`st_blocks × 512`) is what everything counts: sparse
  files count their real allocation, not their length.
- **Apparent** size (`st_size`) is given for files, for display.
- Hard links count once per filesystem: one link is *charged*, the others
  report `bytes: 0`. When the charged link goes away another one is charged.
- A folder's `total` includes the folder's own inode, its files, and every
  subfolder, even the ones the client isn't allowed to see (names never
  leak; aggregate sizes do, like `df` already does).
- Folders never cross into another mount: each filesystem is counted on its
  own.

Times (`mtime`, `last_full_scan`) are Unix seconds.

## Requests

### Hello

Check the daemon is there and what it can do. Optional, but a good first
message.

```json
→ {"v":1,"type":"Hello","client":"sentinelle"}
← {"v":1,"type":"Hello","daemon":"repertoire 0.1.0","protocol":1,"capabilities":["filesystems","tree","children","top","search","rescan","subscribe","live"],"uid":1000}
```

| field | meaning |
|---|---|
| `client` | Optional, free text, logged with `--verbose`. |
| `capabilities` | Requests this daemon answers. `live` is present when it watches with fanotify (otherwise numbers come from periodic rescans). |
| `uid` | The uid answers are filtered for (the peer's, from the kernel). |

### Filesystems

Every indexed filesystem with its state.

```json
→ {"v":1,"type":"Filesystems"}
← {"v":1,"type":"Filesystems","filesystems":[{"id":"/dev/nvme0n1p2","device":"nvme0n1p2","fs_type":"ext4","mount_points":[{"path":"/"}],"total":3998614716416,"used":2015302926336,"indexed":1996150210560,"files":1203337,"dirs":143022,"state":{"state":"live"},"last_full_scan":1792876924}]}
```

| field | meaning |
|---|---|
| `id` | Stable identifier: the backing device, or the path for a directory indexed with `--root`. |
| `device`, `fs_type` | For display. `fs_type` is `dir` for `--root` directories. |
| `mount_points` | Shortest first. One filesystem can be mounted several times (btrfs subvolumes). |
| `total`, `used` | From `statvfs`, as `df` shows them. |
| `indexed` | What the index attributes to paths. `used - indexed` is space nothing can be attributed to: unreadable folders (when not running as root), filesystem metadata, and files deleted but still open. |
| `files`, `dirs` | Entries in the index (mount points count as directories). |
| `state` | See below. |
| `last_full_scan` | When the last full scan finished; absent before the first one. |

`state` is an object with a `state` field:

| state | meaning | extra fields |
|---|---|---|
| `scanning` | First scan, no data yet. `Tree` and friends answer with what's been indexed so far (usually nothing). | `progress` (0–1), `bytes_seen` |
| `reconciling` | Serving a snapshot (after a restart) or the previous scan while a new scan checks it. Numbers are usable. | `progress`, `bytes_seen` |
| `live` | Kept current by kernel events. | |
| `stale` | Events were lost (queue overflow); numbers may be off until the running rescan finishes. | `progress`, `bytes_seen` |
| `static` | Scanned, not watched (no fanotify: not root, or not Linux). Rescanned every 20 minutes. | |

`progress` is `bytes_seen` over what the scan is expected to find (the
previous index's total, or the used space for a first scan); it can stop
short of 1.

### Tree

A pruned folder tree, for size bars. Same shape as sentinelle's
`disk::Node`.

```json
→ {"v":1,"type":"Tree","path":"/home/sigma","min_bytes":500000,"max_depth":3}
← {"v":1,"type":"Tree","root":{"path":"/home/sigma","size":3907584,"own":4096,"children":[{"path":"/home/sigma/Games","size":3002368,"own":0,"children":[{"path":"/home/sigma/Games/steamapps","size":3002368,"own":3002368}]},{"path":"/home/sigma/Videos","size":901120,"own":901120}]}}
```

| request field | default | meaning |
|---|---|---|
| `path` | required | A folder. |
| `min_bytes` | 0 | Folders smaller than this are folded into their parent's `own`. |
| `max_depth` | 64 | Levels below `path` to descend. |

Every node has `size == own + sum(children sizes)`; `own` is the folder's
inode, its files, and the folders folded into it. Children are sorted
largest first; `children` is omitted when empty. Answers over 200 000
folders are refused with `too_large`.

### Children

One folder's direct contents, for a file explorer.

```json
→ {"v":1,"type":"Children","path":"/home/sigma","files":true,"sort":"size","limit":2,"offset":0}
← {"v":1,"type":"Children","path":"/home/sigma","total":3907584,"hidden_bytes":0,"dir_count":3,"file_count":1,"dirs":[{"name":"Games","total":3002368,"files":1,"dirs":3,"mtime":1792876920},{"name":"Videos","total":901120,"files":1,"dirs":0,"mtime":1792876920}],"files":[{"name":"notes.txt","kind":"file","bytes":4096,"apparent":120,"mtime":1792876920}]}
```

| request field | default | meaning |
|---|---|---|
| `path` | required | A folder. |
| `files` | false | Also list non-folders. |
| `sort` | `name` | `name` (byte order), `size` (largest first) or `mtime` (newest first). Applies to folders and files separately. |
| `limit`, `offset` | 1000, 0 | Paging, applied to folders and to files separately. |

| answer field | meaning |
|---|---|
| `total` | The folder's total. |
| `hidden_bytes` | Space inside that the client can't list (the folder isn't readable for it): `total` minus the folder's own inode. 0 otherwise. |
| `dir_count`, `file_count` | Counts before paging. |
| `dirs[]` | `name`, `total`, `files` and `dirs` (recursive counts), `mtime`; `unreadable: true` when the daemon couldn't read it; `mount: "<fs id>"` for a filesystem mounted there (its own totals; not part of this folder's `total`). |
| `files[]` | `name`, `kind` (`file`, `symlink`, `other`), `bytes` (0 for an uncharged hard link), `apparent`, `mtime`. Omitted when `files` is false. |

### Top

The `n` largest folders under `path` that don't contain one another.

```json
→ {"v":1,"type":"Top","path":"/home/sigma","n":2}
← {"v":1,"type":"Top","folders":[{"path":"/home/sigma/Games/steamapps/common/Hades","total":3002368,"files":1},{"path":"/home/sigma/Videos","total":901120,"files":1}]}
```

How they're picked: starting from the subfolders of `path`, a folder whose
largest subfolder holds at least half of it is a *wrapper* and is replaced by
its subfolders (so `Games/steamapps/common` gives way to the games in it);
then, while there are fewer than `n` folders, the largest one that has
subfolders is split into them. The result is sorted largest first. `n`
defaults to 20.

### Search

Find files and folders by name, across every indexed filesystem.

```json
→ {"v":1,"type":"Search","query":"été","limit":5}
← {"v":1,"type":"Search","results":[{"path":"/home/sigma/Été 2024","kind":"dir","bytes":4096,"rank":"prefix"}]}
```

| request field | default | meaning |
|---|---|---|
| `query` | required | Matched against entry names (not whole paths), case-insensitively. |
| `under` | everything | Only below this folder (filesystems mounted below it included). |
| `limit` | 50 | At most 10 000. |
| `kinds` | all | Any of `dir`, `file`, `symlink`, `other`. |

Ranking, best first: `exact` (the whole name), `prefix`, `word` (the match
starts a word: after a separator like `-`, `_`, `.`, space, or at a
lowercase-to-uppercase or letter-to-digit change), then `substring`. Ties
go to shorter names, then larger sizes. `bytes` is a folder's total or a
file's bytes. A search over half a million names takes a few milliseconds.

### Rescan

Start an idle-priority full scan. The current numbers keep being served
(state `reconciling`) until it's done.

```json
→ {"v":1,"type":"Rescan","fs_id":"/dev/nvme0n1p2"}
← {"v":1,"type":"Rescan","started":["/dev/nvme0n1p2"]}
```

Without `fs_id`, every filesystem is rescanned. A scan already queued for a
filesystem isn't queued twice.

### Subscribe

Watch folders. The answer is `Subscribed`, then a stream of events on the
same connection; send nothing more on it.

```json
→ {"v":1,"type":"Subscribe","paths":[{"path":"/"},{"path":"/home"}]}
← {"v":1,"type":"Subscribed"}
← {"v":1,"type":"Changed","path":"/","total_bytes":411236548608}
← {"v":1,"type":"Changed","path":"/home","total_bytes":1996150210560}
← {"v":1,"type":"FsState","fs_id":"/dev/nvme0n1p2","state":{"state":"live"}}
← {"v":1,"type":"Changed","path":"/home","total_bytes":1996153356288}
```

- `Changed` is sent for a watched folder when its total differs from the
  last value sent, at most once a second per folder. The first one for each
  folder comes right away, so a client gets current values without asking.
  Anything below the folder changing changes its total.
- `FsState` is sent for every filesystem when its state changes, and every
  second while a scan runs (progress).
- Paths the client may not see are silently skipped.
- The stream ends when the daemon stops; clients should reconnect.

## Errors

Any request can be answered with an error instead; the connection stays
usable.

```json
← {"v":1,"type":"Error","code":"not_found","message":"/nope is not indexed (or not visible to you)"}
```

| code | meaning |
|---|---|
| `bad_request` | Malformed JSON, unknown `type`, relative path, `..`. |
| `version` | `v` isn't 1. |
| `not_found` | The path isn't indexed, isn't a folder, or the client may not see it (deliberately the same answer). |
| `too_large` | A `Tree` answer would exceed 200 000 folders. |
| `internal` | A bug. |

Lines longer than 1 MiB close the connection.

## Access control

The daemon runs as root and indexes everything. For each connection it reads
the peer's uid and groups from the kernel (`SO_PEERCRED` and
`/proc/<pid>/status`), never from the client. For uid 0 everything is
visible. For anyone else, an entry is visible when the client could find it
itself:

- every folder above it is searchable (`x`) for the client, and
- the folder containing it is readable and searchable (`r` and `x`).

A folder the client can't read still appears in its parent's listing (with
its total), but nothing inside it does: not in `Children`, `Tree`, `Top`,
`Search` or `Subscribe`. Asking for a path the client can't reach gives
`not_found`.

Permission bits are checked with the owner/group/other rules. **ACLs,
capabilities and LSMs (SELinux, AppArmor) are ignored in version 1**: a
folder opened to someone through an ACL looks closed to them, and one
closed by an ACL looks open (its names visible).
