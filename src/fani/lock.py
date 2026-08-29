"""A per-repository advisory lock.

The skill's ``state.json`` is written load-modify-save with no lock of its own,
so two concurrent runs silently drop each other's records. A scheduled job that
overruns its interval would do exactly that, so fani refuses to start a second
run rather than corrupting the translation memory.

The lock is a file created with ``O_EXCL`` holding the owner's pid. A lock whose
owner is gone is stale and gets cleared, because a killed run must not wedge the
schedule forever.
"""

import errno
import json
import os
import time
from pathlib import Path


class LockBusy(RuntimeError):
    """Another live run holds the lock."""


class Lock:
    def __init__(self, path: Path, stale_after_s: float = 24 * 3600):
        self.path = Path(path)
        self.stale_after_s = stale_after_s
        self.acquired = False

    def _owner_alive(self) -> bool:
        try:
            data = json.loads(self.path.read_text(encoding="utf-8"))
            pid = int(data["pid"])
            started = float(data.get("started", 0))
        except (OSError, ValueError, KeyError, TypeError):
            return False  # unreadable lock cannot prove an owner; treat as stale
        if time.time() - started > self.stale_after_s:
            return False
        if pid == os.getpid():
            return True
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return False
        except PermissionError:
            return True  # exists, owned by another user
        return True

    def acquire(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        payload = json.dumps({"pid": os.getpid(), "started": time.time()})
        try:
            fd = os.open(self.path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o644)
        except OSError as exc:
            if exc.errno != errno.EEXIST:
                raise
            if self._owner_alive():
                raise LockBusy(f"another run holds {self.path}") from None
            self.path.unlink(missing_ok=True)
            try:
                fd = os.open(self.path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o644)
            except OSError:
                raise LockBusy(f"another run holds {self.path}") from None
        with os.fdopen(fd, "w") as fh:
            fh.write(payload)
        self.acquired = True

    def release(self) -> None:
        if self.acquired:
            self.path.unlink(missing_ok=True)
            self.acquired = False

    def __enter__(self) -> "Lock":
        self.acquire()
        return self

    def __exit__(self, *exc) -> None:
        self.release()
