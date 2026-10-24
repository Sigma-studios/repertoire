# -*- coding: utf-8 -*-
# Find files and folders by name through the répertoire daemon.
#
# The daemon keeps every file name in memory and answers a search in a few
# milliseconds; this plugin only talks to its socket (newline-delimited
# JSON, see docs/PROTOCOL.md in the répertoire repository).

import base64
import json
import os
import socket
from pathlib import Path

from albert import *

md_iid = "5.0"
md_version = "1.0"
md_name = "Répertoire"
md_description = "Find files and folders by name, instantly"
md_license = "MIT"
md_url = "https://github.com/Sigma-studios/repertoire"
md_authors = ["@Sigma-dev"]
md_platforms = ["Linux"]

SOCKET = os.environ.get("REPERTOIRE_SOCKET", "/run/repertoire/repertoire.sock")
LIMIT = 40


class DaemonError(Exception):
    pass


def request(message, socket_path=None, timeout=2.0):
    """Send one request, return the decoded answer (raises DaemonError)."""
    socket_path = socket_path or SOCKET
    message = dict(message, v=1)
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.settimeout(timeout)
            s.connect(socket_path)
            s.sendall(json.dumps(message).encode() + b"\n")
            buf = b""
            while not buf.endswith(b"\n"):
                chunk = s.recv(65536)
                if not chunk:
                    break
                buf += chunk
    except OSError as e:
        raise DaemonError(f"répertoire isn't running ({e.strerror or e})")
    try:
        answer = json.loads(buf)
    except ValueError:
        raise DaemonError("unexpected answer from répertoire")
    if answer.get("type") == "Error":
        raise DaemonError(answer.get("message", "error"))
    return answer


def wire_path(obj, field="path"):
    """Paths are plain strings, or base64 bytes when not valid UTF-8."""
    if field in obj:
        return obj[field]
    raw = base64.b64decode(obj.get(field + "_b64", ""))
    return os.fsdecode(raw)


def human(n):
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if n < 1024 or unit == "TiB":
            return f"{n:.0f} {unit}" if unit == "B" or n >= 100 else f"{n:.1f} {unit}"
        n /= 1024


def search(query, socket_path=None, limit=LIMIT):
    """[(path, kind, bytes, rank)] best first."""
    answer = request({"type": "Search", "query": query, "limit": limit}, socket_path)
    return [(wire_path(r), r["kind"], r["bytes"], r["rank"]) for r in answer.get("results", [])]


# Rank from the daemon, as a score Albert can mix with usage history.
SCORES = {"exact": 1.0, "prefix": 0.8, "word": 0.6, "substring": 0.4}


def _actions(path, kind):
    parent = str(Path(path).parent)
    actions = [Action("open", "Open", lambda p=path: openFile(p))]
    if kind != "dir":
        actions.append(Action("folder", "Open containing folder", lambda p=parent: openFile(p)))
    actions.append(Action("copy", "Copy path", lambda p=path: setClipboardText(p)))
    return actions


def make_item(path, kind, size, rank):
    name = Path(path).name or path
    folder = str(Path(path).parent)
    return StandardItem(
        id=path,
        text=name + ("/" if kind == "dir" else ""),
        subtext=f"{human(size)} · {folder}",
        icon_factory=lambda p=path: Icon.fileType(p),
        actions=_actions(path, kind),
        input_action_text=name,
    )


class Plugin(PluginInstance, GeneratorQueryHandler):

    def __init__(self):
        PluginInstance.__init__(self)
        GeneratorQueryHandler.__init__(self)

    def defaultTrigger(self):
        return "f "

    def synopsis(self, query):
        return "file or folder name"

    def items(self, ctx):
        query = ctx.query.strip()
        if len(query) < 2:
            return
        try:
            hits = search(query)
        except DaemonError as e:
            yield [StandardItem(id="repertoire.error", text="Répertoire", subtext=str(e),
                                icon_factory=lambda: Icon.grapheme("⚠️"))]
            return
        if not ctx.isValid:
            return
        yield [make_item(*h) for h in hits]
