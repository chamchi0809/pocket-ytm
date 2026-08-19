#!/usr/bin/env python3
"""Capture YouTube Music browser authentication from a dedicated Chrome profile."""

from __future__ import annotations

import json
import os
import platform
import shutil
import subprocess
import tempfile
import time
import urllib.request
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

import websocket


MUSIC_LIBRARY_URL = "https://music.youtube.com/library"
BROWSE_PATH = "/youtubei/v1/browse"
REQUIRED_HEADERS = {"authorization", "cookie", "x-goog-authuser"}
START_TIMEOUT_SECONDS = 15
LOGIN_TIMEOUT_SECONDS = 5 * 60
MAX_PENDING_REQUESTS = 128


class ChromeLoginError(RuntimeError):
    pass


def is_music_browse_request(url: str, method: str) -> bool:
    parsed = urlsplit(url)
    return (
        method.upper() == "POST"
        and parsed.scheme == "https"
        and parsed.hostname == "music.youtube.com"
        and parsed.path == BROWSE_PATH
    )


def headers_as_text(headers: dict[str, Any]) -> str | None:
    normalized = {
        str(name).lower(): str(value)
        for name, value in headers.items()
        if not str(name).startswith(":") and value is not None
    }
    if not REQUIRED_HEADERS.issubset(normalized):
        return None
    return "\n".join(f"{name}: {value}" for name, value in normalized.items())


class BrowseRequestCollector:
    """Correlate CDP request and extra-info events without retaining other traffic."""

    def __init__(self) -> None:
        self._browse_requests: set[str] = set()
        self._headers: dict[str, dict[str, Any]] = {}

    def ingest(self, event: dict[str, Any]) -> str | None:
        method = event.get("method")
        params = event.get("params")
        if not isinstance(params, dict):
            return None
        request_id = str(params.get("requestId") or "")
        if not request_id:
            return None

        if method == "Network.requestWillBeSent":
            request = params.get("request")
            if not isinstance(request, dict) or not is_music_browse_request(
                str(request.get("url") or ""), str(request.get("method") or "")
            ):
                self._headers.pop(request_id, None)
                return None
            self._browse_requests.add(request_id)
            self._merge(request_id, request.get("headers"))
        elif method == "Network.requestWillBeSentExtraInfo":
            self._merge(request_id, params.get("headers"))
        else:
            return None

        if request_id not in self._browse_requests:
            return None
        return headers_as_text(self._headers.get(request_id, {}))

    def _merge(self, request_id: str, raw_headers: Any) -> None:
        if not isinstance(raw_headers, dict):
            return
        if request_id not in self._headers and len(self._headers) >= MAX_PENDING_REQUESTS:
            oldest = next(
                (
                    pending
                    for pending in self._headers
                    if pending not in self._browse_requests
                ),
                next(iter(self._headers)),
            )
            self._headers.pop(oldest, None)
        current = self._headers.setdefault(request_id, {})
        current.update(raw_headers)


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


def _wait_for_debug_port(
    profile: Path, process: subprocess.Popen[bytes], timeout: float
) -> int:
    active_port = profile / "DevToolsActivePort"
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise ChromeLoginError("로그인용 브라우저가 시작 직후 종료되었습니다.")
        try:
            first_line = active_port.read_text(encoding="utf-8").splitlines()[0]
            return int(first_line)
        except (OSError, IndexError, ValueError):
            time.sleep(0.05)
    raise ChromeLoginError("로그인용 브라우저의 연결 정보를 찾지 못했습니다.")


def _wait_for_page_target(
    port: int, process: subprocess.Popen[bytes], timeout: float
) -> str:
    deadline = time.monotonic() + timeout
    endpoint = f"http://127.0.0.1:{port}/json/list"
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise ChromeLoginError("로그인용 브라우저가 종료되었습니다.")
        try:
            with urllib.request.urlopen(endpoint, timeout=0.5) as response:
                targets = json.load(response)
            pages = [target for target in targets if target.get("type") == "page"]
            preferred = next(
                (
                    page
                    for page in pages
                    if str(page.get("url") or "").startswith(
                        ("https://music.youtube.com", "https://accounts.google.com")
                    )
                ),
                pages[0] if pages else None,
            )
            if preferred and preferred.get("webSocketDebuggerUrl"):
                return str(preferred["webSocketDebuggerUrl"])
        except (OSError, ValueError, json.JSONDecodeError):
            pass
        time.sleep(0.05)
    raise ChromeLoginError("로그인용 브라우저 탭에 연결하지 못했습니다.")


def _stop_browser(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def capture_browser_auth_headers(timeout: float = LOGIN_TIMEOUT_SECONDS) -> str:
    browser = find_chromium_browser()
    with tempfile.TemporaryDirectory(
        prefix="pocket-music-login-", ignore_cleanup_errors=True
    ) as profile_raw:
        profile = Path(profile_raw)
        command = [
            browser,
            "--remote-debugging-port=0",
            "--remote-allow-origins=http://localhost",
            f"--user-data-dir={profile}",
            "--no-first-run",
            "--no-default-browser-check",
            "--new-window",
            MUSIC_LIBRARY_URL,
        ]
        process = subprocess.Popen(
            command,
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
            connection.send(
                json.dumps({"id": 1, "method": "Network.enable", "params": {}})
            )
            collector = BrowseRequestCollector()
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise ChromeLoginError("로그인용 브라우저가 닫혀 로그인을 취소했습니다.")
                try:
                    event = json.loads(connection.recv())
                except websocket.WebSocketTimeoutException:
                    continue
                except websocket.WebSocketConnectionClosedException as exc:
                    raise ChromeLoginError(
                        "로그인용 브라우저 탭이 닫혀 로그인을 취소했습니다."
                    ) from exc
                except (json.JSONDecodeError, TypeError):
                    continue
                if headers := collector.ingest(event):
                    return headers
            raise ChromeLoginError(
                "5분 안에 YouTube Music 로그인을 확인하지 못했습니다. 다시 시도해 주세요."
            )
        finally:
            if connection is not None:
                try:
                    connection.close()
                except OSError:
                    pass
            _stop_browser(process)
