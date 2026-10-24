"""Tests the plugin without Albert: a stub `albert` module and a fake daemon.

    python3 -m unittest discover -s albert/tests
"""

import json
import os
import socket
import sys
import tempfile
import threading
import types
import unittest
from pathlib import Path

# Stub just enough of Albert's v5 Python interface.
albert = types.ModuleType("albert")


class _Base:
    def __init__(self, *a, **k):
        pass


class StandardItem:
    def __init__(self, id=None, text=None, subtext=None, icon_factory=None, actions=None, input_action_text=None):
        self.id, self.text, self.subtext, self.actions = id, text, subtext, actions or []
        self.input_action_text = input_action_text
        self.icon = icon_factory()


class Action:
    def __init__(self, id, text, callable):
        self.id, self.text, self.callable = id, text, callable


class Icon:
    fileType = staticmethod(lambda p: ("filetype", str(p)))
    grapheme = staticmethod(lambda g: ("grapheme", g))


opened, copied = [], []
albert.PluginInstance = type("PluginInstance", (_Base,), {})
albert.GeneratorQueryHandler = type("GeneratorQueryHandler", (_Base,), {})
albert.StandardItem = StandardItem
albert.Action = Action
albert.Icon = Icon
albert.openFile = opened.append
albert.setClipboardText = copied.append
albert.__all__ = [n for n in vars(albert) if not n.startswith("_")]
sys.modules["albert"] = albert

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import repertoire as plugin  # noqa: E402


class FakeDaemon(threading.Thread):
    """Answers each request line with `reply(request)`, any number of them
    per connection, and counts connections."""

    def __init__(self, path, reply):
        super().__init__(daemon=True)
        self.reply, self.requests, self.connections = reply, [], 0
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.bind(path)
        self.sock.listen()

    def run(self):
        while True:
            conn, _ = self.sock.accept()
            self.connections += 1
            with conn, conn.makefile() as lines:
                for line in lines:
                    req = json.loads(line)
                    self.requests.append(req)
                    conn.sendall(json.dumps(self.reply(req)).encode() + b"\n")


# A small indexed tree for path queries: folder -> ([(name, total)], [(name, bytes)]).
TREE = {
    "/home/s/Projects": ([("bevy_rues", 9 << 30), ("Bevy_Rues_old", 1 << 30), ("sentinelle", 1 << 20)], []),
    "/home/s/Projects/bevy_rues": ([("target", 4 << 30), ("src", 1 << 20), ("assets", 2 << 20)],
                                   [("test.txt", 4096), ("Cargo.toml", 4096)]),
    "/old/bevy_rues": ([("tools", 1 << 20)], []),
}


def tree_reply(req):
    if req["type"] == "Search":
        # Folders whose name matches, ranked like the daemon (exact first).
        results = []
        for folder, (dirs, _) in TREE.items():
            for name, total in dirs:
                r = plugin.rank(name, req["query"])
                if r:
                    results.append({"path": f"{folder}/{name}", "kind": "dir", "bytes": total, "rank": r})
        for folder in TREE:
            name = folder.rsplit("/", 1)[1]
            if plugin.rank(name, req["query"]) and not any(r["path"] == folder for r in results):
                results.append({"path": folder, "kind": "dir", "bytes": 0, "rank": plugin.rank(name, req["query"])})
        results.sort(key=lambda r: plugin.ORDER[r["rank"]])
        return {"v": 1, "type": "Search", "results": results[: req["limit"]]}
    if req["type"] == "Children":
        if req["path"] not in TREE:
            return {"v": 1, "type": "Error", "code": "not_found", "message": "not indexed"}
        dirs, files = TREE[req["path"]]
        answer = {"v": 1, "type": "Children", "path": req["path"],
                  "dirs": [{"name": n, "total": t, "files": 0, "dirs": 0, "mtime": 0} for n, t in dirs]}
        if req["files"]:
            answer["files"] = [{"name": n, "kind": "file", "bytes": b, "apparent": b, "mtime": 0} for n, b in files]
        return answer
    return {"v": 1, "type": "Error", "code": "bad_request", "message": "?"}


class Ctx:
    def __init__(self, query):
        self.query, self.isValid = query, True


class PluginTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp()
        self.path = os.path.join(self.dir, "s.sock")
        plugin.SOCKET = self.path

    def run_query(self, q):
        items = []
        for batch in plugin.Plugin().items(Ctx(q)):
            items.extend(batch)
        return items

    def test_results_become_items(self):
        bad = os.fsencode("/home/s/caf\udce9")  # not UTF-8: travels as base64
        import base64
        d = FakeDaemon(self.path, lambda req: {"v": 1, "type": "Search", "results": [
            {"path": "/home/s/Steam", "kind": "dir", "bytes": 3 << 30, "rank": "exact"},
            {"path_b64": base64.b64encode(bad).decode(), "kind": "file", "bytes": 10, "rank": "substring"},
        ]})
        d.start()
        items = self.run_query("steam")
        self.assertEqual(d.requests[0], {"type": "Search", "query": "steam", "limit": plugin.LIMIT, "v": 1})
        self.assertEqual(items[0].text, "Steam/")
        self.assertEqual(items[0].subtext, "3.0 GiB · /home/s")
        self.assertEqual(items[1].id, os.fsdecode(bad))
        # Actions: folders open, files also offer their folder; both copy.
        self.assertEqual([a.id for a in items[0].actions], ["open", "copy"])
        self.assertEqual([a.id for a in items[1].actions], ["open", "folder", "copy"])
        items[1].actions[1].callable()
        self.assertEqual(opened[-1], "/home/s")
        items[0].actions[1].callable()
        self.assertEqual(copied[-1], "/home/s/Steam")
        # Tab on a folder goes into it as a path query; on a file, just the name.
        self.assertEqual(items[0].input_action_text, "Steam/")
        self.assertEqual(items[1].input_action_text, os.path.basename(os.fsdecode(bad)))

    def test_path_query(self):
        d = FakeDaemon(self.path, tree_reply)
        d.start()
        items = self.run_query("bevy_rues/t")
        # Only folders named exactly bevy_rues are followed (not Bevy_Rues_old),
        # both of them; then what's inside matches loosely, best first:
        # prefix, then word start (Cargo.toml), then substring (assets).
        self.assertEqual([i.id for i in items],
                         ["/old/bevy_rues/tools", "/home/s/Projects/bevy_rues/target", "/home/s/Projects/bevy_rues/test.txt",
                          "/home/s/Projects/bevy_rues/Cargo.toml", "/home/s/Projects/bevy_rues/assets"])
        self.assertEqual(items[1].text, "target/")
        self.assertEqual(items[1].subtext, "4.0 GiB · /home/s/Projects/bevy_rues")
        self.assertEqual(items[1].input_action_text, "bevy_rues/target/")
        self.assertEqual(items[2].input_action_text, "bevy_rues/test.txt")
        # A search for the anchor and one listing per match, on one connection.
        self.assertEqual(d.connections, 1)
        self.assertEqual([r["type"] for r in d.requests], ["Search", "Children", "Children"])
        self.assertEqual(d.requests[0]["kinds"], ["dir"])

    def test_nested_path_query(self):
        FakeDaemon(self.path, tree_reply).start()
        # Folder names before a slash are exact, in any case, and consecutive.
        self.assertEqual([i.id for i in self.run_query("projects/BEVY_RUES/sr")], ["/home/s/Projects/bevy_rues/src"])
        self.assertEqual(self.run_query("Projects/bevy_rue/t"), [])
        # A trailing slash lists the folder, largest first among equals.
        self.assertEqual([i.text for i in self.run_query("Projects/bevy_rues/")][:2], ["target/", "assets/"])

    def test_path_query_daemon_down(self):
        items = self.run_query("bevy_rues/t")
        self.assertEqual(len(items), 1)
        self.assertIn("isn't running", items[0].subtext)

    def test_real_paths(self):
        root = Path(self.dir) / "tree"
        (root / "target" / "debug").mkdir(parents=True)
        (root / "tests").mkdir()
        (root / "notes.txt").write_bytes(b"x" * 5000)
        os.symlink("notes.txt", root / "tmp-link")
        # No daemon: listed from the filesystem, folders without totals.
        items = self.run_query(f"{root}/t")
        self.assertEqual([i.text for i in items], ["tests/", "target/", "tmp-link", "notes.txt"])
        self.assertEqual(items[0].subtext, str(root))
        self.assertEqual(items[1].input_action_text, f"{root}/target/")
        self.assertTrue(items[3].subtext.endswith(f"KiB · {root}"))
        # Nothing for a folder that doesn't exist.
        self.assertEqual(self.run_query(f"{root}/nope/x"), [])
        # With the daemon, folder totals come from it.
        FakeDaemon(self.path, lambda req: {"v": 1, "type": "Children", "path": req["path"],
                                           "dirs": [{"name": "target", "total": 7 << 30, "files": 0, "dirs": 0, "mtime": 0}]}).start()
        items = self.run_query(f"{root}/tar")
        self.assertEqual(items[0].subtext, f"7.0 GiB · {root}")

    def test_home_paths(self):
        home = Path(self.dir) / "home"
        (home / "Projects").mkdir(parents=True)
        old = os.environ.get("HOME")
        os.environ["HOME"] = str(home)
        try:
            items = self.run_query("~/Pro")
        finally:
            os.environ["HOME"] = old
        self.assertEqual([i.id for i in items], [str(home / "Projects")])
        self.assertEqual(items[0].input_action_text, "~/Projects/")

    def test_ranks_match_the_daemon(self):
        # The same cases as repertoire-core's query tests.
        r = plugin.rank
        self.assertEqual(r("Steam", "steam"), "exact")
        self.assertEqual(r("steamapps", "steam"), "prefix")
        self.assertEqual(r("my-steam-games", "steam"), "word")
        self.assertEqual(r("MySteam", "steam"), "word")
        self.assertEqual(r("mysteam", "steam"), "substring")
        self.assertEqual(r("mysteam_steam", "steam"), "word")
        self.assertIsNone(r("stea", "steam"))
        self.assertEqual(r("Été 2024.mp4", "été"), "prefix")
        self.assertEqual(r("photos-ÉTÉ", "été"), "word")
        self.assertEqual(r("anything", ""), "prefix")

    def test_non_utf8_paths_travel_as_base64(self):
        bad = os.fsdecode(b"/home/caf\xe9")
        self.assertEqual(plugin.path_field("/home/s"), {"path": "/home/s"})
        self.assertEqual(plugin.path_field(bad), {"path_b64": "L2hvbWUvY2Fm6Q=="})

    def test_daemon_down_is_one_item(self):
        items = self.run_query("steam")
        self.assertEqual(len(items), 1)
        self.assertIn("isn't running", items[0].subtext)

    def test_short_queries_ask_nothing(self):
        d = FakeDaemon(self.path, lambda req: {"v": 1, "type": "Search", "results": []})
        d.start()
        self.assertEqual(self.run_query("s"), [])
        self.assertEqual(d.requests, [])

    def test_daemon_errors_show(self):
        FakeDaemon(self.path, lambda req: {"v": 1, "type": "Error", "code": "internal", "message": "boom"}).start()
        self.assertEqual(self.run_query("steam")[0].subtext, "boom")

    def test_sizes(self):
        self.assertEqual(plugin.human(512), "512 B")
        self.assertEqual(plugin.human(1536), "1.5 KiB")
        self.assertEqual(plugin.human(200 << 20), "200 MiB")


if __name__ == "__main__":
    unittest.main()
