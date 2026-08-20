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


def resolved_search_media(info: Any) -> dict[str, Any]:
    if not isinstance(info, dict):
        raise ValueError("YouTube 검색 결과가 올바르지 않습니다")
    entries = info.get("entries")
    if not isinstance(entries, list):
        entries = list(entries or [])
    for entry in entries:
        try:
            result = resolved_media(entry)
        except ValueError:
            continue
        result["videoId"] = str(entry.get("id") or "")
        result["title"] = str(entry.get("title") or "")
        return result
    raise ValueError("재생 가능한 YouTube 대체 영상을 찾지 못했습니다")


def resolved_playlist(info: Any, requested_id: str) -> dict[str, Any]:
    if not isinstance(info, dict):
        raise ValueError("YouTube 플레이리스트 결과가 올바르지 않습니다")
    entries = info.get("entries")
    if not isinstance(entries, list):
        entries = list(entries or [])
    items: list[dict[str, Any]] = []
    for source_index, entry in enumerate(entries):
        if not isinstance(entry, dict):
            continue
        video_id = str(entry.get("id") or "").strip()
        title = str(entry.get("title") or "").strip()
        if not video_id or not title:
            continue
        duration = entry.get("duration")
        if not isinstance(duration, (int, float)) or duration <= 0:
            duration = None
        thumbnail = str(entry.get("thumbnail") or "").strip()
        if not thumbnail:
            thumbnails = entry.get("thumbnails")
            if isinstance(thumbnails, list):
                thumbnail = next(
                    (
                        str(candidate.get("url") or "").strip()
                        for candidate in reversed(thumbnails)
                        if isinstance(candidate, dict) and candidate.get("url")
                    ),
                    "",
                )
        items.append(
            {
                "id": video_id,
                "kind": "song",
                "title": title,
                "subtitle": str(
                    entry.get("channel") or entry.get("uploader") or "YouTube"
                ),
                "videoId": video_id,
                "sourcePlaylistId": str(info.get("id") or requested_id),
                "sourceIndex": source_index,
                "thumbnail": thumbnail or None,
                "durationSeconds": round(duration) if duration is not None else None,
                "available": True,
            }
        )
    if not items:
        raise ValueError("YouTube 플레이리스트에 재생 가능한 영상이 없습니다")
    return {
        "playlistId": str(info.get("id") or requested_id),
        "items": items,
    }


class ResolverService:
    def __init__(
        self,
        deno: str | None,
        cookies: str | None,
        ydl_factory: Callable[[dict[str, Any]], Any] = YoutubeDL,
        pot_provider: str | None = None,
    ) -> None:
        self.deno = deno
        self.cookies = Path(cookies).expanduser() if cookies else None
        self.pot_provider = (
            Path(pot_provider).expanduser().resolve() if pot_provider else None
        )
        self.ydl_factory = ydl_factory
        self.ydl: Any = None
        self.current_format: str | None = None
        self.current_profile: tuple[bool, str, int] | None = None
        self.cookie_signature: tuple[int, int] | None = None
        self.cache: OrderedDict[
            tuple[str, str, int], tuple[float, dict[str, Any]]
        ] = OrderedDict()

    def _current_cookie_signature(self) -> tuple[int, int] | None:
        try:
            stat = self.cookies.stat() if self.cookies else None
            return (stat.st_mtime_ns, stat.st_size) if stat else None
        except OSError:
            return None

    def _options(
        self,
        format_selector: str,
        authenticated: bool,
        player_client: str,
        playlist_limit: int = 0,
    ) -> dict[str, Any]:
        signature = self._current_cookie_signature()
        extractor_args = {"youtube": {"player_client": [player_client]}}
        if player_client == "mweb" and self.pot_provider:
            extractor_args["youtubepot-bgutilscript"] = {
                "server_home": [str(self.pot_provider)]
            }
        options: dict[str, Any] = {
            "quiet": True,
            "no_warnings": True,
            "skip_download": True,
            "noplaylist": True,
            "ignoreconfig": True,
            "logger": QuietLogger(),
            "format": format_selector,
            "extractor_args": extractor_args,
        }
        if self.deno:
            options["js_runtimes"] = {"deno": {"path": self.deno}}
        if authenticated and signature is not None and self.cookies:
            options["cookiefile"] = str(self.cookies)
        if playlist_limit > 0:
            options["extract_flat"] = "in_playlist"
            options["playlistend"] = playlist_limit
        return options

    def _ensure_ydl(
        self,
        format_selector: str,
        authenticated: bool,
        player_client: str,
        playlist_limit: int = 0,
    ) -> Any:
        signature = self._current_cookie_signature()
        if signature != self.cookie_signature:
            self.cookie_signature = signature
            self.cache.clear()
        profile = (authenticated, player_client, playlist_limit)
        if (
            self.ydl is not None
            and format_selector == self.current_format
            and profile == self.current_profile
        ):
            return self.ydl
        if self.ydl is not None:
            self.ydl.close()
        options = self._options(
            format_selector, authenticated, player_client, playlist_limit
        )
        self.ydl = self.ydl_factory(options)
        self.current_format = format_selector
        self.current_profile = profile
        return self.ydl

    def _profiles(self) -> list[tuple[bool, str]]:
        # WEB_EMBEDDED_PLAYER usually exposes the smallest repeatable audio-only
        # stream. Some official music uploads reject that URL outright; Android's
        # combined format remains range-addressable for those videos. Profiles
        # known to fail after their first byte range (android_vr, mweb and
        # tv_embedded) are deliberately excluded.
        #
        # Account cookies remain isolated to the YT Music metadata bridge instead
        # of poisoning either public audio transport profile.
        return [(False, "web_embedded"), (False, "android")]

    def resolve(
        self, url: str, format_selector: str, profile_index: int = 0
    ) -> dict[str, Any]:
        profiles = self._profiles()
        if profile_index < 0 or profile_index >= len(profiles):
            raise ValueError("요청한 YouTube 재생 프로필이 없습니다")
        key = (url, format_selector, profile_index)
        now = time.monotonic()
        cached = self.cache.get(key)
        if cached and now - cached[0] < CACHE_TTL_SECONDS:
            self.cache.move_to_end(key)
            return cached[1]
        self.cache.pop(key, None)

        authenticated, player_client = profiles[profile_index]
        ydl = self._ensure_ydl(format_selector, authenticated, player_client)
        result = resolved_media(ydl.extract_info(url, download=False))
        self.cache[key] = (now, result)
        self.cache.move_to_end(key)
        while len(self.cache) > CACHE_CAPACITY:
            self.cache.popitem(last=False)
        return result

    def search_resolve(
        self, query: str, format_selector: str, profile_index: int = 0
    ) -> dict[str, Any]:
        query = " ".join(query.split()).strip()
        if not query:
            raise ValueError("YouTube 대체 검색어가 없습니다")
        if len(query) > 500:
            raise ValueError("YouTube 대체 검색어가 너무 깁니다")
        profiles = self._profiles()
        if profile_index < 0 or profile_index >= len(profiles):
            raise ValueError("요청한 YouTube 재생 프로필이 없습니다")
        cache_key = (f"search:{query}", format_selector, profile_index)
        now = time.monotonic()
        cached = self.cache.get(cache_key)
        if cached and now - cached[0] < CACHE_TTL_SECONDS:
            self.cache.move_to_end(cache_key)
            return cached[1]
        self.cache.pop(cache_key, None)

        authenticated, player_client = profiles[profile_index]
        ydl = self._ensure_ydl(format_selector, authenticated, player_client)
        result = resolved_search_media(
            ydl.extract_info(f"ytsearch1:{query}", download=False)
        )
        self.cache[cache_key] = (now, result)
        self.cache.move_to_end(cache_key)
        while len(self.cache) > CACHE_CAPACITY:
            self.cache.popitem(last=False)
        return result

    def playlist_resolve(
        self, playlist_id: str, limit: int = 50, profile_index: int = 0
    ) -> dict[str, Any]:
        playlist_id = playlist_id.strip()
        allowed = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_"
        if not playlist_id or any(character not in allowed for character in playlist_id):
            raise ValueError("YouTube 플레이리스트 ID가 올바르지 않습니다")
        limit = max(1, min(int(limit), 100))
        profiles = self._profiles()
        if profile_index < 0 or profile_index >= len(profiles):
            raise ValueError("요청한 YouTube 재생 프로필이 없습니다")
        cache_key = (f"playlist:{playlist_id}:{limit}", "flat", profile_index)
        now = time.monotonic()
        cached = self.cache.get(cache_key)
        if cached and now - cached[0] < CACHE_TTL_SECONDS:
            self.cache.move_to_end(cache_key)
            return cached[1]
        self.cache.pop(cache_key, None)

        authenticated, player_client = profiles[profile_index]
        ydl = self._ensure_ydl(
            DEFAULT_FORMAT, authenticated, player_client, playlist_limit=limit
        )
        result = resolved_playlist(
            ydl.extract_info(
                f"https://www.youtube.com/playlist?list={playlist_id}", download=False
            ),
            playlist_id,
        )
        self.cache[cache_key] = (now, result)
        self.cache.move_to_end(cache_key)
        while len(self.cache) > CACHE_CAPACITY:
            self.cache.popitem(last=False)
        return result

    def handle(self, request: Any) -> dict[str, Any]:
        if not isinstance(request, dict):
            raise ValueError("resolver 요청은 JSON 객체여야 합니다")
        operation = request.get("op")
        if operation == "ping":
            authenticated, player_client = self._profiles()[0]
            self._ensure_ydl(DEFAULT_FORMAT, authenticated, player_client)
            return {"ready": True, "version": YT_DLP_VERSION}
        if operation not in ("resolve", "searchResolve", "playlistResolve"):
            raise ValueError(f"지원하지 않는 resolver 요청입니다: {operation}")
        format_selector = str(request.get("format") or DEFAULT_FORMAT)
        profile_index = int(request.get("profile") or 0)
        if operation == "playlistResolve":
            return self.playlist_resolve(
                str(request.get("playlistId") or ""),
                int(request.get("limit") or 50),
                profile_index,
            )
        if operation == "searchResolve":
            return self.search_resolve(
                str(request.get("query") or ""), format_selector, profile_index
            )
        url = str(request.get("url") or "")
        if not url.startswith(("https://", "http://")):
            raise ValueError("resolver URL이 올바르지 않습니다")
        return self.resolve(url, format_selector, profile_index)

    def close(self) -> None:
        if self.ydl is not None:
            self.ydl.close()
            self.ydl = None
            self.current_format = None
            self.current_profile = None


def concise_error(error: Exception) -> str:
    lines = [line.strip() for line in str(error).splitlines() if line.strip()]
    return lines[-1] if lines else error.__class__.__name__


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Pocket Music persistent yt-dlp resolver")
    parser.add_argument("--deno")
    parser.add_argument("--cookies")
    parser.add_argument("--pot-provider")
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
    service = ResolverService(args.deno, args.cookies, pot_provider=args.pot_provider)
    try:
        run(service)
    finally:
        service.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
