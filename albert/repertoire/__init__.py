# -*- coding: utf-8 -*-
# Find files and folders by name through the répertoire daemon.
#
# The daemon keeps every file name in memory and answers a search in a few
# milliseconds; this plugin only talks to its socket (newline-delimited
# JSON, see docs/PROTOCOL.md in the répertoire repository).
#
# Queries with a slash are paths. Every segment before the last slash is an
# exact folder name (any case) and only the last one matches loosely, so
# `bevy_rues/t` lists what's in folders named bevy_rues that matches "t".
# The first folder is found by a search, the rest by listing one level at a
# time: a few small requests, not a scan. A query starting with / or ~ is a
# real path and is listed from the filesystem (sizes from the daemon if it
# runs). Tab on a folder completes it with a slash, to go one level down.

import base64
import json
import os
import socket
from pathlib import Path

from albert import *

md_iid = "5.0"
md_version = "1.1"
md_name = "Répertoire"
md_description = "Find files and folders by name, instantly"
md_license = "MIT"
md_url = "https://github.com/Sigma-studios/repertoire"
md_authors = ["@Sigma-dev"]
md_platforms = ["Linux"]

SOCKET = os.environ.get("REPERTOIRE_SOCKET", "/run/repertoire/repertoire.sock")
LIMIT = 40
# Folders followed per path segment. Only exact names are followed, so this
# only bites on common names (`src/`, `target/`); the largest win ties.
ANCHORS = 20
# Entries asked for per listed folder.
LISTING = 5000


class DaemonError(Exception):
    pass


class Connection:
    """One socket for several requests, since a path query makes a few."""

    def __init__(self, socket_path=None, timeout=2.0):
        try:
            self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            self.sock.settimeout(timeout)
            self.sock.connect(socket_path or SOCKET)
        except OSError as e:
            self.sock.close()
            raise DaemonError(f"répertoire isn't running ({e.strerror or e})")
        self.buf = b""

    def request(self, message):
        """Send one request, return the decoded answer (raises DaemonError)."""
        try:
            self.sock.sendall(json.dumps(dict(message, v=1)).encode() + b"\n")
            while b"\n" not in self.buf:
                chunk = self.sock.recv(65536)
                if not chunk:
                    break
                self.buf += chunk
        except OSError as e:
            raise DaemonError(f"répertoire isn't answering ({e.strerror or e})")
        line, _, self.buf = self.buf.partition(b"\n")
        try:
            answer = json.loads(line)
        except ValueError:
            raise DaemonError("unexpected answer from répertoire")
        if answer.get("type") == "Error":
            raise DaemonError(answer.get("message", "error"))
        return answer

    def close(self):
        self.sock.close()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


def request(message, socket_path=None, timeout=2.0):
    """One request on its own connection."""
    with Connection(socket_path, timeout) as c:
        return c.request(message)


def wire_path(obj, field="path"):
    """Paths are plain strings, or base64 bytes when not valid UTF-8."""
    if field in obj:
        return obj[field]
    raw = base64.b64decode(obj.get(field + "_b64", ""))
    return os.fsdecode(raw)


def path_field(path, field="path"):
    """The other direction: {field: str}, or {field_b64: …} for raw bytes."""
    try:
        path.encode("utf-8")
        return {field: path}
    except UnicodeEncodeError:  # surrogate-escaped bytes from os.fsdecode
        return {field + "_b64": base64.b64encode(os.fsencode(path)).decode()}


def rank(name, query):
    """How `query` matches `name`, like the daemon's Search ranks it."""
    n, q = name.lower(), query.lower()
    if not q:
        return "prefix"
    best = None
    p = n.find(q)
    while p != -1:
        if p == 0:
            return "exact" if len(q) == len(n) else "prefix"
        if p < len(name) and _word_start(name, p):
            return "word"
        best = "substring"
        p = n.find(q, p + 1)
    return best


def _word_start(name, p):
    """After a separator, a lower-to-upper change or a letter-to-digit one."""
    a, b = name[p - 1], name[p]
    return (a.isascii() and not a.isalnum()) or (a.islower() and b.isupper()) or (a.isalpha() and b.isdigit())


ORDER = {"exact": 0, "prefix": 1, "word": 2, "substring": 3}


def best_first(hits, query):
    """Daemon order: rank, then shorter names, then larger sizes. With
    nothing after the slash it's a listing, so just largest first."""
    if not query:
        return sorted(hits, key=lambda h: -(h[2] or 0))
    return sorted(hits, key=lambda h: (ORDER[h[3]], len(Path(h[0]).name), -(h[2] or 0)))


def human(n):
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if n < 1024 or unit == "TiB":
            return f"{n:.0f} {unit}" if unit == "B" or n >= 100 else f"{n:.1f} {unit}"
        n /= 1024


def search(query, socket_path=None, limit=LIMIT):
    """[(path, kind, bytes, rank)] best first."""
    answer = request({"type": "Search", "query": query, "limit": limit}, socket_path)
    return [(wire_path(r), r["kind"], r["bytes"], r["rank"]) for r in answer.get("results", [])]


def _children(conn, folder, files):
    answer = conn.request(dict(path_field(folder), type="Children", files=files, sort="size", limit=LISTING))
    dirs = [(os.path.join(folder, wire_path(d, "name")), "dir", d["total"]) for d in answer.get("dirs", [])]
    rest = [(os.path.join(folder, wire_path(f, "name")), f["kind"], f["bytes"]) for f in answer.get("files", [])]
    return dirs + rest


def search_path(query, socket_path=None, limit=LIMIT):
    """`a/b/c`: folders named a (anywhere), then b directly inside, then
    whatever in those matches c. Same result shape as search()."""
    *folders, last = query.split("/")
    folders = [f for f in folders if f]  # `a//b` is `a/b`
    with Connection(socket_path) as conn:
        answer = conn.request({"type": "Search", "query": folders[0], "limit": ANCHORS, "kinds": ["dir"]})
        found = [wire_path(r) for r in answer.get("results", []) if r["rank"] == "exact"]
        for name in folders[1:]:
            name = name.lower()
            found = [p for f in found for p, kind, _ in _children(conn, f, files=False)
                     if Path(p).name.lower() == name][:ANCHORS]
        hits = []
        for folder in found:
            for path, kind, size in _children(conn, folder, files=True):
                r = rank(Path(path).name, last)
                if r:
                    hits.append((path, kind, size, r))
    return best_first(hits, last)[:limit]


def list_real(query, socket_path=None, limit=LIMIT):
    """`/x/y/z` or `~/y/z`: what's in /x/y (or ~/y) matching z, read from the
    filesystem. Folder totals come from the daemon when it runs."""
    base, _, last = query.rpartition("/")
    folder = os.path.expanduser(base or "/")
    try:
        entries = list(os.scandir(folder))
    except OSError:
        return []
    totals = {}
    try:
        with Connection(socket_path, timeout=0.5) as conn:
            totals = {p: size for p, kind, size in _children(conn, folder, files=False)}
    except DaemonError:
        pass
    hits = []
    for e in entries:
        r = rank(e.name, last)
        if not r:
            continue
        if e.is_dir(follow_symlinks=False):
            hits.append((e.path, "dir", totals.get(e.path), r))
        else:
            try:
                st = e.stat(follow_symlinks=False)
                size = st.st_blocks * 512  # allocated, as the daemon counts
            except OSError:
                size = None
            hits.append((e.path, "symlink" if e.is_symlink() else "file", size, r))
    return best_first(hits, last)[:limit]


# Rank from the daemon, as a score Albert can mix with usage history.
SCORES = {"exact": 1.0, "prefix": 0.8, "word": 0.6, "substring": 0.4}


def _actions(path, kind):
    parent = str(Path(path).parent)
    actions = [Action("open", "Open", lambda p=path: openFile(p))]
    if kind != "dir":
        actions.append(Action("folder", "Open containing folder", lambda p=parent: openFile(p)))
    actions.append(Action("copy", "Copy path", lambda p=path: setClipboardText(p)))
    return actions


def make_item(path, kind, size, rank, typed=""):
    """`typed` is the query up to its last slash, so Tab keeps the path."""
    name = Path(path).name or path
    folder = str(Path(path).parent)
    return StandardItem(
        id=path,
        text=name + ("/" if kind == "dir" else ""),
        subtext=f"{human(size)} · {folder}" if size is not None else folder,
        icon_factory=lambda p=path: Icon.fileType(p),
        actions=_actions(path, kind),
        input_action_text=typed + name + ("/" if kind == "dir" else ""),
    )


class Plugin(PluginInstance, GeneratorQueryHandler):

    def __init__(self):
        PluginInstance.__init__(self)
        GeneratorQueryHandler.__init__(self)

    def defaultTrigger(self):
        return "f "

    def synopsis(self, query):
        return "name, or folder/name"

    def items(self, ctx):
        query = ctx.query.strip()
        typed = query.rpartition("/")[0] + "/" if "/" in query else ""
        try:
            if query.startswith(("/", "~")):
                hits = list_real(query)
            elif "/" in query:
                hits = search_path(query)
            elif len(query) >= 2:
                hits = search(query)
            else:
                return
        except DaemonError as e:
            yield [StandardItem(id="repertoire.error", text="Répertoire", subtext=str(e),
                                icon_factory=lambda: Icon.grapheme("⚠️"))]
            return
        if not ctx.isValid:
            return
        yield [make_item(*h, typed=typed) for h in hits]
