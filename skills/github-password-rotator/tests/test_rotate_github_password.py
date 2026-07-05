import importlib.util
import os
import subprocess
import sys
import unittest
from pathlib import Path


SCRIPT = (
    Path(__file__).resolve().parents[1]
    / "scripts"
    / "rotate_github_password.py"
)
SPEC = importlib.util.spec_from_file_location("rotate_github_password", SCRIPT)
module = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
sys.modules[SPEC.name] = module
SPEC.loader.exec_module(module)


class GithubPasswordRotatorTest(unittest.TestCase):
    def test_parse_args_uses_environment_password_sources(self):
        args = module.parse_args(
            [
                "--github-login",
                "octo",
                "--current-password-env",
                "OLD_ENV",
                "--new-password-env",
                "NEW_ENV",
            ]
        )

        self.assertEqual(args.github_login, "octo")
        self.assertEqual(args.current_password_env, "OLD_ENV")
        self.assertEqual(args.new_password_env, "NEW_ENV")
        self.assertFalse(hasattr(args, "current_password"))
        self.assertFalse(hasattr(args, "new_password"))

    def test_parse_args_supports_login_only_mode(self):
        args = module.parse_args(["--github-login", "octo", "--login-only"])

        self.assertTrue(args.login_only)
        self.assertFalse(args.create_learning_repo)

    def test_resolve_secret_reads_environment_without_printing_value(self):
        old_value = os.environ.get("GH_PASSWORD_TEST")
        os.environ["GH_PASSWORD_TEST"] = "secret-value"
        try:
            value = module.resolve_secret("GH_PASSWORD_TEST", "ignored")
        finally:
            if old_value is None:
                os.environ.pop("GH_PASSWORD_TEST", None)
            else:
                os.environ["GH_PASSWORD_TEST"] = old_value

        self.assertEqual(value, "secret-value")

    def test_start_browser_helper_passes_passwords_only_via_environment(self):
        calls = []

        class FakeProcess:
            pass

        def fake_which(name):
            if name == "node":
                return "/usr/bin/node"
            return None

        def fake_popen(cmd, env, **kwargs):
            calls.append((cmd, env, kwargs))
            return FakeProcess()

        args = module.parse_args(["--github-login", "octo"])
        original_which = module.shutil.which
        original_popen = module.subprocess.Popen
        module.shutil.which = fake_which
        module.subprocess.Popen = fake_popen
        try:
            process = module.start_browser_helper(
                args,
                9222,
                current_password="old-secret",
                new_password="new-secret",
            )
        finally:
            module.shutil.which = original_which
            module.subprocess.Popen = original_popen

        self.assertIsInstance(process, FakeProcess)
        self.assertEqual(len(calls), 1)
        cmd, env, _kwargs = calls[0]
        self.assertEqual(cmd, ["node", str(module.NODE_DRIVER)])
        self.assertNotIn("old-secret", cmd)
        self.assertNotIn("new-secret", cmd)
        self.assertEqual(env["GITHUB_LOGIN"], "octo")
        self.assertEqual(env["GITHUB_CURRENT_PASSWORD"], "old-secret")
        self.assertEqual(env["GITHUB_NEW_PASSWORD"], "new-secret")
        self.assertEqual(env["GITHUB_CREATE_LEARNING_REPO"], "0")
        self.assertEqual(env["GITHUB_AUTO_2FA_FUN"], "0")
        self.assertEqual(env["GITHUB_TOTP_SECRET"], "")
        self.assertEqual(env["GITHUB_LOGIN_ONLY"], "0")

    def test_start_browser_helper_passes_login_only_mode_to_browser_helper(self):
        calls = []

        class FakeProcess:
            pass

        def fake_which(name):
            if name == "node":
                return "/usr/bin/node"
            return None

        def fake_popen(cmd, env, **kwargs):
            calls.append((cmd, env, kwargs))
            return FakeProcess()

        args = module.parse_args(["--github-login", "octo", "--login-only"])
        original_which = module.shutil.which
        original_popen = module.subprocess.Popen
        module.shutil.which = fake_which
        module.subprocess.Popen = fake_popen
        try:
            module.start_browser_helper(
                args,
                9222,
                current_password="old-secret",
                new_password="",
            )
        finally:
            module.shutil.which = original_which
            module.subprocess.Popen = original_popen

        self.assertEqual(len(calls), 1)
        cmd, env, _kwargs = calls[0]
        self.assertNotIn("old-secret", cmd)
        self.assertEqual(env["GITHUB_LOGIN_ONLY"], "1")
        self.assertEqual(env["GITHUB_NEW_PASSWORD"], "")

    def test_create_learning_repo_flag_is_passed_to_browser_helper(self):
        calls = []

        class FakeProcess:
            pass

        def fake_which(name):
            if name == "node":
                return "/usr/bin/node"
            return None

        def fake_popen(cmd, env, **kwargs):
            calls.append((cmd, env, kwargs))
            return FakeProcess()

        args = module.parse_args(["--github-login", "octo", "--create-learning-repo"])
        original_which = module.shutil.which
        original_popen = module.subprocess.Popen
        module.shutil.which = fake_which
        module.subprocess.Popen = fake_popen
        try:
            module.start_browser_helper(
                args,
                9222,
                current_password="old-secret",
                new_password="new-secret",
            )
        finally:
            module.shutil.which = original_which
            module.subprocess.Popen = original_popen

        self.assertEqual(len(calls), 1)
        _cmd, env, _kwargs = calls[0]
        self.assertEqual(env["GITHUB_CREATE_LEARNING_REPO"], "1")
        self.assertEqual(env["GITHUB_LEARNING_REPO_OWNER"], "octo")

    def test_auto_2fa_fun_secret_is_passed_only_via_environment(self):
        calls = []

        class FakeProcess:
            pass

        def fake_which(name):
            if name == "node":
                return "/usr/bin/node"
            return None

        def fake_popen(cmd, env, **kwargs):
            calls.append((cmd, env, kwargs))
            return FakeProcess()

        args = module.parse_args(["--github-login", "octo", "--auto-2fa-fun"])
        original_which = module.shutil.which
        original_popen = module.subprocess.Popen
        module.shutil.which = fake_which
        module.subprocess.Popen = fake_popen
        try:
            module.start_browser_helper(
                args,
                9222,
                current_password="old-secret",
                new_password="new-secret",
                totp_secret="totp-secret",
            )
        finally:
            module.shutil.which = original_which
            module.subprocess.Popen = original_popen

        self.assertEqual(len(calls), 1)
        cmd, env, _kwargs = calls[0]
        self.assertNotIn("totp-secret", cmd)
        self.assertEqual(env["GITHUB_AUTO_2FA_FUN"], "1")
        self.assertEqual(env["GITHUB_TOTP_SECRET"], "totp-secret")

    def test_dry_run_does_not_launch_browser_or_require_secrets(self):
        result = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--github-login",
                "octo",
                "--dry-run",
                "--manual-timeout-seconds",
                "1",
            ],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("dry_run", result.stdout)
        self.assertIn("octo", result.stdout)

    def test_dry_run_login_only_does_not_require_new_password(self):
        result = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--github-login",
                "octo",
                "--login-only",
                "--dry-run",
            ],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn('"login_only": true', result.stdout)


if __name__ == "__main__":
    unittest.main()
