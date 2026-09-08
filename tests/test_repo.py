from __future__ import annotations

from dataclasses import replace
import unittest

from homestack import models, repo
from support import test_config


class RepositoryHelpersTests(unittest.TestCase):
    def test_repository_argument_uses_explicit_value_or_config_default(self) -> None:
        cfg = replace(
            test_config(),
            repo_default_repository="example/default",
        )
        self.assertEqual(
            repo.resolve_repository_argument(
                cfg, "hipotures/tklivetracker"
            ),
            "hipotures/tklivetracker",
        )
        self.assertEqual(
            repo.resolve_repository_argument(cfg, None),
            "example/default",
        )
        with self.assertRaisesRegex(models.AppError, "default_repository"):
            repo.resolve_repository_argument(test_config(), None)

    def test_repository_paths_use_persistent_home(self) -> None:
        checkout, key, public_key = repo.repository_paths(
            test_config(), "hipotures/tklivetracker"
        )
        self.assertEqual(
            checkout, "/home/user/DEV/tklivetracker"
        )
        self.assertEqual(
            key,
            "/home/user/.ssh/homestack/github/hipotures-tklivetracker",
        )
        self.assertEqual(public_key, key + ".pub")

    def test_supported_github_remotes_resolve_to_owner_repo(self) -> None:
        expected = "hipotures/tklivetracker"
        for value in (
            "git@github.com:hipotures/tklivetracker.git",
            "git@github.com:hipotures/tklivetracker",
            "https://github.com/hipotures/tklivetracker.git",
            "ssh://git@github.com/hipotures/tklivetracker.git",
        ):
            with self.subTest(value=value):
                self.assertEqual(
                    repo.repository_from_remote(value), expected
                )
        self.assertIsNone(
            repo.repository_from_remote(
                "git@example.com:hipotures/tklivetracker.git"
            )
        )


class RepositorySetupPlanTests(unittest.TestCase):
    def _state(self, **updates: object) -> dict[str, object]:
        state: dict[str, object] = {
            "checkout_state": "ready",
            "key_state": "ready",
            "deploy_key_state": "read-write",
            "origin": "git@github.com:hipotures/tklivetracker.git",
            "ssh_config_state": "ready",
            "access": "working",
        }
        state.update(updates)
        return state

    def test_ready_repository_is_noop(self) -> None:
        self.assertEqual(
            repo.repository_setup_actions(self._state()), ()
        )

    def test_existing_https_checkout_is_not_recloned(self) -> None:
        actions = repo.repository_setup_actions(
            self._state(
                key_state="missing",
                deploy_key_state="missing",
                origin="https://github.com/hipotures/tklivetracker.git",
                ssh_config_state="missing",
                access="not-tested",
            )
        )
        self.assertEqual(
            actions,
            (
                "generate-key",
                "register-key",
                "set-origin",
                "set-ssh-command",
                "verify-access",
            ),
        )
        self.assertNotIn("clone", actions)

    def test_missing_checkout_is_cloned_without_destructive_actions(self) -> None:
        actions = repo.repository_setup_actions(
            self._state(
                checkout_state="missing",
                key_state="missing",
                deploy_key_state="missing",
                origin=None,
                ssh_config_state="missing",
                access="not-tested",
            )
        )
        self.assertIn("clone", actions)
        self.assertNotIn("reset", actions)
        self.assertNotIn("clean", actions)

    def test_conflicting_checkout_is_rejected(self) -> None:
        for checkout_state in (
            "not-a-repository",
            "repository-without-recognized-origin",
            "different-repository",
        ):
            with self.subTest(checkout_state=checkout_state):
                with self.assertRaisesRegex(
                    models.AppError,
                    "will not clean, reset, or replace",
                ):
                    repo.repository_setup_actions(
                        self._state(checkout_state=checkout_state)
                    )


if __name__ == "__main__":
    unittest.main()
