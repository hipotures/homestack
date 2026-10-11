"""Audible cues and failure classification for hardware security key waits."""
from __future__ import annotations

from contextlib import contextmanager
from dataclasses import dataclass, field
from pathlib import Path
import os
import shutil
import subprocess
import threading
from typing import Callable, Iterator

# ssh gives up on an unconfirmed key touch after about 30 seconds (measured on a
# YubiKey), so the cue repeats at 8, 16 and 24 seconds and the failure cue
# follows when the attempt ends.
REPEAT_SECONDS = 8.0
TIMEOUT_SECONDS = 30.0
INITIAL_DELAY_SECONDS = 1.0

SOUND_DIR = Path("/usr/share/sounds/freedesktop/stereo")
DEFAULT_FILES = {"wait": "window-attention.oga", "ok": "complete.oga", "fail": "dialog-error.oga"}
# Players tried in order; each entry is the command prefix that precedes the sound file.
PLAYERS: tuple[tuple[str, ...], ...] = (("pw-play",), ("paplay",), ("canberra-gtk-play", "-f"))


@dataclass(frozen=True)
class Sounds:
    """Cue configuration. A missing player or file falls back to the terminal bell."""

    enabled: bool = True
    player: tuple[str, ...] = ()
    files: dict[str, str] = field(default_factory=lambda: dict(DEFAULT_FILES))
    repeat_seconds: float = REPEAT_SECONDS
    initial_delay: float = INITIAL_DELAY_SECONDS

    def command(self, event: str) -> list[str] | None:
        """Return the argv that plays one cue, or None when no player/file is available."""
        name = self.files.get(event)
        if not name:
            return None
        path = Path(name).expanduser()
        if not path.is_absolute():
            path = SOUND_DIR / name
        if not path.is_file():
            return None
        prefix = self.player or next((p for p in PLAYERS if shutil.which(p[0])), ())
        if not prefix or shutil.which(prefix[0]) is None:
            return None
        return [*prefix, str(path)]


def _ring_bell() -> None:
    try:
        fd = os.open("/dev/tty", os.O_WRONLY | os.O_NOCTTY)
    except OSError:
        return
    try:
        os.write(fd, b"\a")
    except OSError:
        pass
    finally:
        os.close(fd)


def play(sounds: Sounds, event: str, *, spawn: Callable[..., object] = subprocess.Popen,
         bell: Callable[[], None] = _ring_bell) -> None:
    """Play one cue without blocking; never raises."""
    if not sounds.enabled:
        return
    argv = sounds.command(event)
    try:
        if argv is None:
            bell()
            return
        process = spawn(argv, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL, start_new_session=True)
        waiter = getattr(process, "wait", None)
        if waiter is not None:
            # Reap the player so finished cues do not linger as zombies.
            threading.Thread(target=waiter, daemon=True).start()
    except OSError:
        bell()


class TouchWatch:
    """Cue schedule for one wait on a security key.

    The attention cue starts after ``initial_delay`` (a connection that needs no
    touch stays silent), repeats every ``repeat_seconds`` up to ``TIMEOUT_SECONDS``,
    and ``finish`` ends it with a success cue (only if the user was alerted) or a
    failure cue.
    """

    def __init__(self, label: str, sounds: Sounds | None = None, *,
                 player: Callable[[Sounds, str], None] | None = None,
                 timeout: float = TIMEOUT_SECONDS) -> None:
        self.label = label
        self.sounds = sounds or Sounds()
        # Resolved per call so tests (and callers) can replace touch.play.
        self._player = player or (lambda sounds, event: play(sounds, event))
        self._timeout = timeout
        self._done = threading.Event()
        self._lock = threading.Lock()
        self._alerted = False
        self._finished = False
        self._thread: threading.Thread | None = None

    def start(self) -> TouchWatch:
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()
        return self

    def _run(self) -> None:
        if self._done.wait(self.sounds.initial_delay):
            return
        elapsed = self.sounds.initial_delay
        while True:
            with self._lock:
                if self._finished:
                    return
                self._alerted = True
                self._player(self.sounds, "wait")
            if self._done.wait(self.sounds.repeat_seconds):
                return
            elapsed += self.sounds.repeat_seconds
            if elapsed >= self._timeout:
                return

    def finish(self, ok: bool) -> None:
        """End the wait; only the first call plays a cue."""
        with self._lock:
            if self._finished:
                return
            self._finished = True
            self._done.set()
            if not ok:
                self._player(self.sounds, "fail")
            elif self._alerted:
                self._player(self.sounds, "ok")
        if self._thread is not None and self._thread is not threading.current_thread():
            self._thread.join(timeout=1.0)


@contextmanager
def touch_wait(label: str, sounds: Sounds | None = None, **options) -> Iterator[TouchWatch]:
    """Cue a hardware-key wait; an exception (or ``watch.finish(False)``) plays the failure cue."""
    watch = TouchWatch(label, sounds, **options).start()
    try:
        yield watch
    except BaseException:
        watch.finish(False)
        raise
    else:
        watch.finish(True)


# Failure classes for an ssh attempt that used a security key.
ABSENT = "absent"
NOT_CONFIRMED = "not-confirmed"
DENIED = "denied"
FAILED = "failed"

MESSAGES = {
    ABSENT: "Security key not found; plug it in and retry.",
    NOT_CONFIRMED: "Key touch was not confirmed in time.",
    DENIED: "The server rejected the key.",
    FAILED: "SSH authentication failed.",
}


def classify_ssh_failure(returncode: int, elapsed: float, stderr: str) -> str:
    """Name why an ssh attempt with a security key failed.

    A missing device fails at once with "device not found"; an unconfirmed touch
    runs to ssh's own timeout (about 30 s) and then reports a signing failure.
    """
    text = (stderr or "").lower()
    if "device not found" in text:
        return ABSENT
    if "signing failed" in text and elapsed >= TIMEOUT_SECONDS - 5:
        return NOT_CONFIRMED
    if "permission denied" in text:
        return DENIED
    return FAILED
