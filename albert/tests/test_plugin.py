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
    """Answers each request line with `reply(request)`."""

    def __init__(self, path, reply):
        super().__init__(daemon=True)
        self.reply, self.requests = reply, []
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.bind(path)
        self.sock.listen()

    def run(self):
        while True:
            conn, _ = self.sock.accept()
            with conn:
                line = conn.makefile().readline()
                req = json.loads(line)
                self.requests.append(req)
                conn.sendall(json.dumps(self.reply(req)).encode() + b"\n")


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
