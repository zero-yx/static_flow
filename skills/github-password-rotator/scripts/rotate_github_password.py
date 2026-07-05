#!/usr/bin/env python3
"""Assist GitHub password rotation in an isolated browser.

Passwords are accepted only through environment variables or hidden TTY
prompts. They are passed to the DevTools helper through environment variables,
never command-line arguments, and are not printed.
"""

from __future__ import annotations

import argparse
import getpass
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path
from typing import Any


DEFAULT_PROXY = "http://127.0.0.1:11111"
DEFAULT_SETTINGS_URL = "https://github.com/settings/security"
NODE_DRIVER = Path(__file__).with_name("drive_github_password_change.mjs")


def log(message: str) -> None:
    print(message, flush=True)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--github-login", required=True, help="GitHub username or email")
    parser.add_argument("--proxy", default=DEFAULT_PROXY)
    parser.add_argument("--settings-url", default=DEFAULT_SETTINGS_URL)
    parser.add_argument("--current-password-env", default="GITHUB_CURRENT_PASSWORD")
    parser.add_argument("--new-password-env", default="GITHUB_NEW_PASSWORD")
    parser.add_argument("--totp-secret-env", default="GITHUB_TOTP_SECRET")
    parser.add_argument("--manual-timeout-seconds", type=int, default=900)
    parser.add_argument("--chrome-bin")
    parser.add_argument("--chrome-profile")
    parser.add_argument("--debug-port", type=int)
    parser.add_argument("--keep-browser", action="store_true")
    parser.add_argument(
        "--login-only",
        action="store_true",
        help="Log in and stop on the GitHub security settings page without changing the password",
    )
    parser.add_argument(
        "--create-learning-repo",
        action="store_true",
        help="Create a beginner learning repository after the password change completes",
    )
    parser.add_argument(
        "--auto-2fa-fun",
        action="store_true",
        help="Use a TOTP secret with 2fa.fun to fill GitHub 2FA prompts automatically",
    )
    parser.add_argument("--dry-run", action="store_true")
    return parser.parse_args(argv)


def resolve_secret(env_name: str, prompt: str) -> str:
    value = os.environ.get(env_name)
    if value:
        return value
    if sys.stdin.isatty():
        value = getpass.getpass(prompt)
        if value:
            return value
    raise SystemExit(f"{env_name} is required")


def chrome_binary(explicit: str | None) -> str:
    if explicit:
        return explicit
    for name in ("google-chrome", "chromium", "chromium-browser"):
        found = shutil.which(name)
        if found:
            return found
    raise RuntimeError("Chrome/Chromium binary not found")


def find_free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def wait_http_json(url: str, timeout: float = 20.0) -> Any:
    deadline = time.monotonic() + timeout
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    while time.monotonic() < deadline:
        try:
            with opener.open(url, timeout=2) as resp:
                return json.loads(resp.read().decode("utf-8"))
        except Exception:
            time.sleep(0.25)
    raise RuntimeError(f"timed out waiting for {url}")


def wait_for_page_target(port: int) -> None:
    pages = wait_http_json(f"http://127.0.0.1:{port}/json/list", timeout=25)
    page = next((item for item in pages if item.get("type") == "page"), None)
    if not page:
        raise RuntimeError("Chrome DevTools page target not found")


def launch_chrome(args: argparse.Namespace) -> tuple[subprocess.Popen[Any], int, str]:
    port = args.debug_port or find_free_port()
    profile_dir = args.chrome_profile or tempfile.mkdtemp(prefix="github-password-rotator-")
    cmd = [
        chrome_binary(args.chrome_bin),
        f"--user-data-dir={profile_dir}",
        f"--proxy-server={args.proxy}",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-background-networking",
        "--disable-gpu",
        "--disable-software-rasterizer",
        "--remote-debugging-address=127.0.0.1",
        f"--remote-debugging-port={port}",
        args.settings_url,
    ]
    proc = subprocess.Popen(
        cmd,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=args.keep_browser,
    )
    return proc, port, profile_dir


def start_browser_helper(
    args: argparse.Namespace,
    port: int,
    *,
    current_password: str,
    new_password: str,
    totp_secret: str = "",
) -> subprocess.Popen[Any]:
    if not NODE_DRIVER.is_file():
        raise FileNotFoundError(f"Node DevTools driver not found: {NODE_DRIVER}")
    if not shutil.which("node"):
        raise RuntimeError("node is required for browser automation")
    env = os.environ.copy()
    env.update(
        {
            "GITHUB_DEVTOOLS_PORT": str(port),
            "GITHUB_LOGIN": args.github_login,
            "GITHUB_CURRENT_PASSWORD": current_password,
            "GITHUB_NEW_PASSWORD": new_password,
            "GITHUB_MANUAL_TIMEOUT_SECONDS": str(args.manual_timeout_seconds),
            "GITHUB_SETTINGS_URL": args.settings_url,
            "GITHUB_CREATE_LEARNING_REPO": "1" if args.create_learning_repo else "0",
            "GITHUB_LEARNING_REPO_OWNER": args.github_login,
            "GITHUB_AUTO_2FA_FUN": "1" if args.auto_2fa_fun else "0",
            "GITHUB_TOTP_SECRET": totp_secret,
            "GITHUB_LOGIN_ONLY": "1" if args.login_only else "0",
        }
    )
    return subprocess.Popen(["node", str(NODE_DRIVER)], env=env)


def dry_run_summary(args: argparse.Namespace) -> dict[str, Any]:
    return {
        "dry_run": True,
        "github_login": args.github_login,
        "proxy": args.proxy,
        "settings_url": args.settings_url,
        "current_password_env": args.current_password_env,
        "new_password_env": args.new_password_env,
        "totp_secret_env": args.totp_secret_env,
        "manual_timeout_seconds": args.manual_timeout_seconds,
        "create_learning_repo": args.create_learning_repo,
        "auto_2fa_fun": args.auto_2fa_fun,
        "login_only": args.login_only,
        "keep_browser": args.keep_browser or args.login_only,
        "driver": str(NODE_DRIVER),
    }


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    if args.login_only:
        args.keep_browser = True
    if args.dry_run:
        log(json.dumps(dry_run_summary(args), ensure_ascii=False, indent=2))
        return 0

    current_password = resolve_secret(
        args.current_password_env,
        f"Current GitHub password for {args.github_login}: ",
    )
    new_password = ""
    if not args.login_only:
        new_password = resolve_secret(
            args.new_password_env,
            f"New GitHub password for {args.github_login}: ",
        )
        if current_password == new_password:
            raise SystemExit("new password must differ from current password")
    totp_secret = ""
    if args.auto_2fa_fun:
        totp_secret = resolve_secret(
            args.totp_secret_env,
            f"TOTP secret for {args.github_login}: ",
        )

    proc: subprocess.Popen[Any] | None = None
    helper: subprocess.Popen[Any] | None = None
    profile_dir: str | None = None
    try:
        proc, port, profile_dir = launch_chrome(args)
        wait_for_page_target(port)
        helper = start_browser_helper(
            args,
            port,
            current_password=current_password,
            new_password=new_password,
            totp_secret=totp_secret,
        )
        code = helper.wait()
        if code != 0:
            raise RuntimeError(f"browser helper failed with code {code}")
        status = "login_completed" if args.login_only else "password_change_completed"
        log(json.dumps({"status": status, "github_login": args.github_login}))
        return 0
    finally:
        if helper and helper.poll() is None:
            helper.terminate()
            try:
                helper.wait(timeout=5)
            except subprocess.TimeoutExpired:
                helper.kill()
        if proc and not args.keep_browser:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
        if profile_dir and not args.keep_browser and not args.chrome_profile:
            shutil.rmtree(profile_dir, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
