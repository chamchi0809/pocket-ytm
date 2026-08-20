#!/usr/bin/env python3
"""Capture YouTube Music auth after a normal, user-driven Chrome login."""

from __future__ import annotations

import json
import os
import platform
import shutil
import sqlite3
import subprocess
import tempfile
import time
import urllib.request
from hashlib import sha1
from pathlib import Path
from typing import Any

import websocket


MUSIC_LIBRARY_URL = "https://music.youtube.com/library"
MUSIC_ORIGIN = "https://music.youtube.com"
START_TIMEOUT_SECONDS = 15
LOGIN_TIMEOUT_SECONDS = 5 * 60
CAPTURE_TIMEOUT_SECONDS = 10
LOGIN_COOKIE = "__Secure-3PAPISID"
GOOGLE_LOGIN_URL_PATTERN = "https://accounts.google.com/%"
LOGIN_DESTINATION_URL_PATTERNS = (
    "https://music.youtube.com/%",
    "https://www.youtube.com/%",
    "https://youtube.com/%",
    "https://myaccount.google.com/%",
    "https://www.google.com/%",
)


class ChromeLoginError(RuntimeError):
    pass


def auth_headers_from_cookies(cookies: Any, timestamp: int | None = None) -> str:
    """Build the browser-auth headers ytmusicapi needs from CDP cookie data."""
    if not isinstance(cookies, list):
        raise ChromeLoginError("Chrome에서 YouTube Music 쿠키를 읽지 못했습니다.")

    values: list[tuple[str, str]] = []
    sapisid = ""
    for cookie in cookies:
        if not isinstance(cookie, dict):
            continue
        name = str(cookie.get("name") or "")
        value = str(cookie.get("value") or "")
        if not name or any(character in name + value for character in "\r\n;"):
            continue
        values.append((name, value))
        if name == LOGIN_COOKIE:
            sapisid = value

    if not sapisid:
        raise ChromeLoginError(
            "YouTube Music 로그인 쿠키가 없습니다. Chrome에서 로그인을 완료한 뒤 다시 시도해 주세요."
        )

    now = int(time.time()) if timestamp is None else timestamp
    digest = sha1(f"{now} {sapisid} {MUSIC_ORIGIN}".encode()).hexdigest()
    cookie_header = "; ".join(f"{name}={value}" for name, value in values)
    return "\n".join(
        (
            f"authorization: SAPISIDHASH {now}_{digest}",
            f"cookie: {cookie_header}",
            "x-goog-authuser: 0",
            f"origin: {MUSIC_ORIGIN}",
            f"x-origin: {MUSIC_ORIGIN}",
        )
    )


def find_chromium_browser() -> str:
    override = os.environ.get("POCKET_YTM_BROWSER")
    if override and Path(override).is_file():
        return override

    system = platform.system()
    candidates: list[str] = []
    if system == "Darwin":
        candidates = [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            str(Path.home() / "Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ]
    elif system == "Windows":
        for base in (
            os.environ.get("PROGRAMFILES"),
            os.environ.get("PROGRAMFILES(X86)"),
            os.environ.get("LOCALAPPDATA"),
        ):
            if base:
                candidates.extend(
                    [
                        str(Path(base) / "Google/Chrome/Application/chrome.exe"),
                        str(Path(base) / "Microsoft/Edge/Application/msedge.exe"),
                    ]
                )
    else:
        for executable in (
            "google-chrome",
            "google-chrome-stable",
            "microsoft-edge",
            "brave-browser",
            "chromium",
            "chromium-browser",
        ):
            if resolved := shutil.which(executable):
                candidates.append(resolved)

    for candidate in candidates:
        if Path(candidate).is_file() and os.access(candidate, os.X_OK):
            return candidate
    raise ChromeLoginError(
        "빠른 로그인에는 Google Chrome, Microsoft Edge, Brave 또는 Chromium이 필요합니다."
    )


def interactive_login_command(browser: str, profile: Path) -> list[str]:
    return [
        browser,
        f"--user-data-dir={profile}",
        "--no-first-run",
        "--no-default-browser-check",
        "--new-window",
        MUSIC_LIBRARY_URL,
    ]


def capture_command(browser: str, profile: Path) -> list[str]:
    return [
        browser,
        "--headless=new",
        "--remote-debugging-port=0",
        "--remote-allow-origins=http://localhost",
        f"--user-data-dir={profile}",
        "--no-first-run",
        "--no-default-browser-check",
        "about:blank",
    ]


def _wait_for_debug_port(
    profile: Path, process: subprocess.Popen[bytes], timeout: float
) -> int:
    active_port = profile / "DevToolsActivePort"
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise ChromeLoginError("로그인 확인용 브라우저가 시작 직후 종료되었습니다.")
        try:
            first_line = active_port.read_text(encoding="utf-8").splitlines()[0]
            return int(first_line)
        except (OSError, IndexError, ValueError):
            time.sleep(0.05)
    raise ChromeLoginError("로그인 확인용 브라우저의 연결 정보를 찾지 못했습니다.")


def _wait_for_page_target(
    port: int, process: subprocess.Popen[bytes], timeout: float
) -> str:
    deadline = time.monotonic() + timeout
    endpoint = f"http://127.0.0.1:{port}/json/list"
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise ChromeLoginError("로그인 확인용 브라우저가 종료되었습니다.")
        try:
            with urllib.request.urlopen(endpoint, timeout=0.5) as response:
                targets = json.load(response)
            page = next(
                (target for target in targets if target.get("type") == "page"), None
            )
            if page and page.get("webSocketDebuggerUrl"):
                return str(page["webSocketDebuggerUrl"])
        except (OSError, ValueError, json.JSONDecodeError):
            pass
        time.sleep(0.05)
    raise ChromeLoginError("로그인 확인용 브라우저 탭에 연결하지 못했습니다.")


def _stop_browser(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def has_youtube_login_cookie(profile: Path) -> bool:
    candidates = (
        profile / "Default/Network/Cookies",
        profile / "Default/Cookies",
    )
    for database in candidates:
        if not database.is_file():
            continue
        try:
            connection = sqlite3.connect(
                f"{database.as_uri()}?mode=ro", uri=True, timeout=0.1
            )
            try:
                found = connection.execute(
                    "SELECT 1 FROM cookies "
                    "WHERE name = ? AND (host_key = ? OR host_key LIKE ?) LIMIT 1",
                    (LOGIN_COOKIE, "youtube.com", "%.youtube.com"),
                ).fetchone()
            finally:
                connection.close()
            if found:
                return True
        except (OSError, sqlite3.Error, ValueError):
            continue
    return False


def has_completed_google_login(profile: Path) -> bool:
    """Detect a post-login redirect without attaching automation to Chrome."""
    database = profile / "Default/History"
    if not database.is_file():
        return False
    try:
        connection = sqlite3.connect(
            f"{database.as_uri()}?mode=ro&immutable=1",
            uri=True,
            timeout=0.1,
        )
        try:
            destination_conditions = " OR ".join(
                "url LIKE ?" for _ in LOGIN_DESTINATION_URL_PATTERNS
            )
            login_time, destination_time = connection.execute(
                "SELECT "
                "MAX(CASE WHEN url LIKE ? THEN last_visit_time END), "
                f"MAX(CASE WHEN {destination_conditions} THEN last_visit_time END) "
                f"FROM urls WHERE url LIKE ? OR {destination_conditions}",
                (GOOGLE_LOGIN_URL_PATTERN,)
                + LOGIN_DESTINATION_URL_PATTERNS
                + (GOOGLE_LOGIN_URL_PATTERN,)
                + LOGIN_DESTINATION_URL_PATTERNS,
            ).fetchone()
        finally:
            connection.close()
    except (OSError, sqlite3.Error, TypeError, ValueError):
        return False
    return bool(login_time and destination_time and destination_time > login_time)


def _wait_for_interactive_login(
    profile: Path, process: subprocess.Popen[bytes], timeout: float
) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if has_youtube_login_cookie(profile) or has_completed_google_login(profile):
            # Let Chrome finish the cookie transaction before terminating the
            # temporary interactive instance and reopening the same profile.
            time.sleep(0.5)
            return
        status = process.poll()
        if status is not None:
            if status:
                raise ChromeLoginError("로그인용 브라우저가 비정상적으로 종료되었습니다.")
            return
        time.sleep(0.2)
    raise ChromeLoginError(
        "5분 안에 YouTube Music 로그인을 확인하지 못했습니다. 다시 시도해 주세요."
    )


def _capture_from_authenticated_profile(
    browser: str, profile: Path, timeout: float
) -> str:
    process = subprocess.Popen(
        capture_command(browser, profile),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    connection: websocket.WebSocket | None = None
    try:
        port = _wait_for_debug_port(profile, process, START_TIMEOUT_SECONDS)
        websocket_url = _wait_for_page_target(port, process, START_TIMEOUT_SECONDS)
        connection = websocket.create_connection(
            websocket_url,
            timeout=1,
            origin="http://localhost",
        )
        connection.send(json.dumps({"id": 1, "method": "Network.enable"}))
        connection.send(
            json.dumps(
                {
                    "id": 2,
                    "method": "Network.getCookies",
                    "params": {"urls": [MUSIC_LIBRARY_URL]},
                }
            )
        )
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise ChromeLoginError("로그인 확인용 브라우저가 종료되었습니다.")
            try:
                event = json.loads(connection.recv())
            except websocket.WebSocketTimeoutException:
                continue
            except websocket.WebSocketConnectionClosedException as exc:
                raise ChromeLoginError("로그인 확인용 브라우저 연결이 끊겼습니다.") from exc
            except (json.JSONDecodeError, TypeError):
                continue
            if event.get("id") != 2:
                continue
            if event.get("error"):
                raise ChromeLoginError("Chrome에서 로그인 쿠키를 읽지 못했습니다.")
            result = event.get("result")
            cookies = result.get("cookies") if isinstance(result, dict) else None
            return auth_headers_from_cookies(cookies)
        raise ChromeLoginError("Chrome 로그인 쿠키 확인 시간이 초과되었습니다.")
    finally:
        if connection is not None:
            try:
                connection.close()
            except OSError:
                pass
        _stop_browser(process)


def capture_browser_auth_headers(timeout: float = LOGIN_TIMEOUT_SECONDS) -> str:
    browser = find_chromium_browser()
    with tempfile.TemporaryDirectory(
        prefix="pocket-music-login-", ignore_cleanup_errors=True
    ) as profile_raw:
        profile = Path(profile_raw)
        interactive = subprocess.Popen(
            interactive_login_command(browser, profile),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            _wait_for_interactive_login(profile, interactive, timeout)
        finally:
            _stop_browser(interactive)
        return _capture_from_authenticated_profile(
            browser, profile, CAPTURE_TIMEOUT_SECONDS
        )
