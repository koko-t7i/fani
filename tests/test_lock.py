import json
import os
import sys
import tempfile
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "src"))

from fani.lock import Lock, LockBusy


class LockTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = Path(self.tmp.name) / "nested" / "fani.lock"
        self.addCleanup(self.tmp.cleanup)

    def test_acquire_creates_file_and_release_removes_it(self):
        lock = Lock(self.path)
        lock.acquire()
        self.assertTrue(self.path.is_file())
        self.assertEqual(json.loads(self.path.read_text())["pid"], os.getpid())
        lock.release()
        self.assertFalse(self.path.exists())

    def test_context_manager_releases(self):
        with Lock(self.path):
            self.assertTrue(self.path.is_file())
        self.assertFalse(self.path.exists())

    def test_second_acquire_by_live_owner_is_busy(self):
        first = Lock(self.path)
        first.acquire()
        self.addCleanup(first.release)
        with self.assertRaises(LockBusy):
            Lock(self.path).acquire()

    def test_dead_owner_is_cleared(self):
        self.path.parent.mkdir(parents=True)
        # pid 2**22 - 1 is above the default pid_max on Linux, so it cannot exist.
        self.path.write_text(json.dumps({"pid": 4194303, "started": time.time()}))
        lock = Lock(self.path)
        lock.acquire()
        self.addCleanup(lock.release)
        self.assertEqual(json.loads(self.path.read_text())["pid"], os.getpid())

    def test_old_lock_is_stale_even_if_pid_is_alive(self):
        self.path.parent.mkdir(parents=True)
        self.path.write_text(json.dumps({"pid": os.getpid(), "started": 0}))
        lock = Lock(self.path, stale_after_s=60)
        lock.acquire()
        self.addCleanup(lock.release)
        self.assertGreater(json.loads(self.path.read_text())["started"], 0)

    def test_unreadable_lock_is_stale(self):
        self.path.parent.mkdir(parents=True)
        self.path.write_text("not json")
        lock = Lock(self.path)
        lock.acquire()
        self.addCleanup(lock.release)
        self.assertEqual(json.loads(self.path.read_text())["pid"], os.getpid())

    def test_release_without_acquire_is_a_no_op(self):
        self.path.parent.mkdir(parents=True)
        self.path.write_text("someone else")
        Lock(self.path).release()
        self.assertEqual(self.path.read_text(), "someone else")


if __name__ == "__main__":
    unittest.main()
