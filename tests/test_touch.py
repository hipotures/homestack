import subprocess
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

from homestack import config as cfgmod, setup_herdr, touch, workspace_ssh
from homestack.models import AppError


class Recorder:
    def __init__(self):
        self.events = []
        self.lock = threading.Lock()

    def __call__(self, sounds, event):
        with self.lock:
            self.events.append(event)


def sounds(**overrides):
    values = {"repeat_seconds": 0.05, "initial_delay": 0.02}
    values.update(overrides)
    return touch.Sounds(**values)


class TouchWatchTests(unittest.TestCase):
    def test_wait_cue_repeats_until_finished_then_plays_success(self):
        rec = Recorder()
        watch = touch.TouchWatch("key", sounds(), player=rec, timeout=5).start()
        time.sleep(0.22)
        watch.finish(True)
        waits = rec.events.count("wait")
        self.assertGreaterEqual(waits, 3)
        self.assertEqual(rec.events[-1], "ok")
        self.assertNotIn("fail", rec.events)
        count = len(rec.events)
        time.sleep(0.12)
        self.assertEqual(len(rec.events), count, "no cues after finish")

    def test_instant_success_is_silent_but_failure_always_cues(self):
        rec = Recorder()
        watch = touch.TouchWatch("key", sounds(initial_delay=1), player=rec).start()
        watch.finish(True)
        self.assertEqual(rec.events, [])
        rec = Recorder()
        watch = touch.TouchWatch("key", sounds(initial_delay=1), player=rec).start()
        watch.finish(False)
        self.assertEqual(rec.events, ["fail"])

    def test_repeats_stop_at_timeout_and_finish_is_idempotent(self):
        rec = Recorder()
        watch = touch.TouchWatch("key", sounds(repeat_seconds=0.03, initial_delay=0.01),
                                 player=rec, timeout=0.1).start()
        time.sleep(0.3)
        waits = rec.events.count("wait")
        self.assertTrue(1 <= waits <= 4, waits)
        watch.finish(False)
        watch.finish(True)
        self.assertEqual(rec.events.count("fail"), 1)
        self.assertNotIn("ok", rec.events)

    def test_context_manager_plays_failure_on_exception(self):
        rec = Recorder()
        with self.assertRaises(RuntimeError):
            with touch.touch_wait("key", sounds(initial_delay=1), player=rec):
                raise RuntimeError("boom")
        self.assertEqual(rec.events, ["fail"])
        rec = Recorder()
        with touch.touch_wait("key", sounds(initial_delay=1), player=rec):
            pass
        self.assertEqual(rec.events, [])


class PlayTests(unittest.TestCase):
    def test_disabled_plays_nothing_and_missing_player_rings_bell(self):
        bells, spawned = [], []
        touch.play(touch.Sounds(enabled=False), "wait", spawn=lambda *a, **k: spawned.append(a), bell=lambda: bells.append(1))
        self.assertEqual((bells, spawned), ([], []))
        touch.play(touch.Sounds(files={"wait": "/nonexistent.oga"}), "wait",
                   spawn=lambda *a, **k: spawned.append(a), bell=lambda: bells.append(1))
        self.assertEqual((bells, spawned), ([1], []))

    def test_command_uses_configured_player_and_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            sound = Path(tmp) / "tick.oga"
            sound.write_bytes(b"x")
            player = Path(tmp) / "fakeplay"
            player.write_text("#!/bin/sh\n")
            player.chmod(0o755)
            cfg = touch.Sounds(player=(str(player),), files={"wait": str(sound)})
            self.assertEqual(cfg.command("wait"), [str(player), str(sound)])
            self.assertIsNone(cfg.command("ok"))
            calls = []

            class Proc:
                def wait(self):
                    return 0

            touch.play(cfg, "wait", spawn=lambda argv, **k: calls.append(argv) or Proc())
            self.assertEqual(calls, [[str(player), str(sound)]])

    def test_spawn_failure_falls_back_to_bell(self):
        bells = []

        def broken(*a, **k):
            raise OSError("no player")

        with tempfile.TemporaryDirectory() as tmp:
            sound = Path(tmp) / "a.oga"
            sound.write_bytes(b"x")
            player = Path(tmp) / "p"
            player.write_text("#!/bin/sh\n")
            player.chmod(0o755)
            touch.play(touch.Sounds(player=(str(player),), files={"wait": str(sound)}), "wait",
                       spawn=broken, bell=lambda: bells.append(1))
        self.assertEqual(bells, [1])


class ClassifyTests(unittest.TestCase):
    def test_failure_classes_follow_measured_ssh_behaviour(self):
        c = touch.classify_ssh_failure
        self.assertEqual(c(255, 0.4, 'sign_and_send_pubkey: signing failed for ED25519-SK "k": device not found'), touch.ABSENT)
        self.assertEqual(c(255, 29.4, 'signing failed for ED25519-SK "k": invalid format\nPermission denied (publickey).'), touch.NOT_CONFIRMED)
        self.assertEqual(c(255, 0.6, "Permission denied (publickey)."), touch.DENIED)
        self.assertEqual(c(255, 1.0, "Connection refused"), touch.FAILED)
        for kind in (touch.ABSENT, touch.NOT_CONFIRMED, touch.DENIED, touch.FAILED):
            self.assertTrue(touch.MESSAGES[kind])


class WiringTests(unittest.TestCase):
    def _cues(self):
        events = []
        return events, mock.patch.object(touch, "play", lambda sounds, event, **k: events.append(event))

    def test_master_connection_plays_failure_cue_and_stays_silent_when_instant(self):
        events, patch = self._cues()
        loud = touch.Sounds(initial_delay=5)
        with patch, mock.patch("subprocess.run", return_value=subprocess.CompletedProcess([], 255)):
            with self.assertRaises(AppError):
                workspace_ssh.WorkspaceSSH("user@192.0.2.1", 1, sounds=loud).__enter__()
        self.assertEqual(events, ["fail"])
        events, patch = self._cues()
        ssh = workspace_ssh.WorkspaceSSH("user@192.0.2.1", 1, sounds=loud)
        with patch, mock.patch("subprocess.run", return_value=subprocess.CompletedProcess([], 0)), \
                mock.patch.object(workspace_ssh, "run_local", return_value=subprocess.CompletedProcess([], 0)):
            ssh.__enter__()
        ssh.opened = False
        ssh.directory.cleanup()
        self.assertEqual(events, [])

    def test_default_workspace_ssh_is_silent(self):
        self.assertFalse(workspace_ssh.WorkspaceSSH("user@192.0.2.1", 1).sounds.enabled)

    def test_herdr_registration_plays_failure_cue_on_nonzero_exit(self):
        events, patch = self._cues()
        loud = touch.Sounds(initial_delay=5)
        with patch, mock.patch("subprocess.run", return_value=subprocess.CompletedProcess([], 1)), \
                mock.patch("sys.stderr"):
            result = setup_herdr.register_desktop(["herdr", "machine", "add"], sounds=loud)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(events, ["fail"])
        events, patch = self._cues()
        with patch, mock.patch("subprocess.run", return_value=subprocess.CompletedProcess([], 0)), \
                mock.patch("sys.stderr"):
            setup_herdr.register_desktop(["herdr", "machine", "add"], sounds=loud)
        self.assertEqual(events, [])


class SoundsConfigTests(unittest.TestCase):
    def test_section_is_optional_validated_and_round_trips(self):
        self.assertEqual(cfgmod._parse_sounds(None), touch.Sounds())
        custom = cfgmod._parse_sounds({"enabled": True, "player": "pw-play", "wait": "bell.oga",
                                        "repeat_seconds": 10})
        self.assertEqual(custom.player, ("pw-play",))
        self.assertEqual(custom.files["wait"], "bell.oga")
        self.assertEqual(custom.files["ok"], touch.DEFAULT_FILES["ok"])
        self.assertEqual(custom.repeat_seconds, 10.0)
        self.assertEqual(cfgmod._sounds_toml(touch.Sounds()), [])
        text = "\n".join(cfgmod._sounds_toml(custom))
        import tomllib
        again = cfgmod._parse_sounds(tomllib.loads(text)["sounds"])
        self.assertEqual(again, custom)
        for bad in ({"enabled": "yes"}, {"player": 5}, {"wait": 1}, {"repeat_seconds": 0}, {"repeat_seconds": True},
                    {"volume": 3}, "text"):
            with self.subTest(bad=bad), self.assertRaises(AppError):
                cfgmod._parse_sounds(bad)


if __name__ == "__main__":
    unittest.main()
