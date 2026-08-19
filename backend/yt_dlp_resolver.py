#!/usr/bin/env python3
"""Long-lived yt-dlp URL resolver using a newline-delimited JSON protocol."""

from __future__ import annotations

import argparse
import json
import sys
import time
from collections import OrderedDict
from pathlib import Path
from typing import Any, Callable

from yt_dlp import YoutubeDL
from yt_dlp.version import __version__ as YT_DLP_VERSION


CACHE_TTL_SECONDS = 10 * 60
CACHE_CAPACITY = 128
DEFAULT_FORMAT = "bestaudio/best"
SAFE_RESPONSE_HEADERS = {
    "accept",
    "accept-language",
    "origin",
    "referer",
    "user-agent",
}


class QuietLogger:
    def debug(self, _message: str) -> None:
        pass

    def info(self, _message: str) -> None:
        pass

    def warning(self, _message: str) -> None:
        pass

    def error(self, _message: str) -> None:
        pass


def safe_headers(raw: Any) -> dict[str, str]:
    if not isinstance(raw, dict):
        return {}
    return {
        str(key): str(value)
        for key, value in raw.items()
        if str(key).lower() in SAFE_RESPONSE_HEADERS and value is not None
    }


def resolved_media(info: Any) -> dict[str, Any]:
    if not isinstance(info, dict):
        raise ValueError("yt-dlp가 올바른 미디어 정보를 반환하지 않았습니다")
    media_url = info.get("url")
    protocol = str(info.get("protocol") or "")
    if not media_url or protocol not in {"http", "https"}:
        raise ValueError(f"직접 스트리밍할 수 없는 오디오 형식입니다: {protocol or 'unknown'}")
    duration = info.get("duration")
    if not isinstance(duration, (int, float)) or duration <= 0:
        duration = None
    return {
        "url": str(media_url),
        "headers": safe_headers(info.get("http_headers")),
        "durationSeconds": duration,
    }


class ResolverService:
    def __init__(
        self,
        deno: str | None,
        cookies: str | None,
        ydl_factory: Callable[[dict[str, Any]], Any] = YoutubeDL,
    ) -> None:
        self.deno = deno
        self.cookies = Path(cookies).expanduser() if cookies else None
        self.ydl_factory = ydl_factory
        self.ydl: Any = None
        self.current_format: str | None = None
        self.cookie_signature: tuple[int, int] | None = None
        self.cache: OrderedDict[tuple[str, str], tuple[float, dict[str, Any]]] = OrderedDict()

    def _current_cookie_signature(self) -> tuple[int, int] | None:
        try:
            stat = self.cookies.stat() if self.cookies else None
            return (stat.st_mtime_ns, stat.st_size) if stat else None
        except OSError:
            return None

    def _ensure_ydl(self, format_selector: str) -> Any:
        signature = self._current_cookie_signature()
        if (
            self.ydl is not None
            and signature == self.cookie_signature
            and format_selector == self.current_format
        ):
            return self.ydl
        if self.ydl is not None:
            self.ydl.close()
        options: dict[str, Any] = {
            "quiet": True,
            "no_warnings": True,
            "skip_download": True,
            "noplaylist": True,
            "ignoreconfig": True,
            "logger": QuietLogger(),
            "format": format_selector,
            "extractor_args": {
                "youtube": {"player_client": ["web_embedded", "default"]}
            },
        }
        if self.deno:
            options["js_runtimes"] = {"deno": {"path": self.deno}}
        if signature is not None and self.cookies:
            options["cookiefile"] = str(self.cookies)
        self.ydl = self.ydl_factory(options)
        self.cookie_signature = signature
        self.current_format = format_selector
        self.cache.clear()
        return self.ydl

    def resolve(self, url: str, format_selector: str) -> dict[str, Any]:
        ydl = self._ensure_ydl(format_selector)
        key = (url, format_selector)
        now = time.monotonic()
        cached = self.cache.get(key)
        if cached and now - cached[0] < CACHE_TTL_SECONDS:
            self.cache.move_to_end(key)
            return cached[1]
        self.cache.pop(key, None)

        result = resolved_media(ydl.extract_info(url, download=False))
        self.cache[key] = (now, result)
        self.cache.move_to_end(key)
        while len(self.cache) > CACHE_CAPACITY:
            self.cache.popitem(last=False)
        return result

    def handle(self, request: Any) -> dict[str, Any]:
        if not isinstance(request, dict):
            raise ValueError("resolver 요청은 JSON 객체여야 합니다")
        operation = request.get("op")
        if operation == "ping":
            self._ensure_ydl(DEFAULT_FORMAT)
            return {"ready": True, "version": YT_DLP_VERSION}
        if operation != "resolve":
            raise ValueError(f"지원하지 않는 resolver 요청입니다: {operation}")
        url = str(request.get("url") or "")
        if not url.startswith(("https://", "http://")):
            raise ValueError("resolver URL이 올바르지 않습니다")
        format_selector = str(request.get("format") or DEFAULT_FORMAT)
        return self.resolve(url, format_selector)

    def close(self) -> None:
        if self.ydl is not None:
            self.ydl.close()
            self.ydl = None
            self.current_format = None


def concise_error(error: Exception) -> str:
    lines = [line.strip() for line in str(error).splitlines() if line.strip()]
    return lines[-1] if lines else error.__class__.__name__


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Pocket Music persistent yt-dlp resolver")
    parser.add_argument("--deno")
    parser.add_argument("--cookies")
    return parser.parse_args(argv)


def run(service: ResolverService) -> None:
    for line in sys.stdin:
        try:
            request = json.loads(line)
            request_id = request.get("id") if isinstance(request, dict) else None
            response = {"id": request_id, "ok": True, "data": service.handle(request)}
        except Exception as error:  # Protocol errors must not terminate the warm process.
            response = {
                "id": locals().get("request_id"),
                "ok": False,
                "error": concise_error(error),
            }
        sys.stdout.write(json.dumps(response, ensure_ascii=False, separators=(",", ":")) + "\n")
        sys.stdout.flush()


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    service = ResolverService(args.deno, args.cookies)
    try:
        run(service)
    finally:
        service.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
