#!/usr/bin/env python3
"""Small, persistent JSON-lines adapter around ytmusicapi.

The Rust process owns this child and communicates only over stdin/stdout.  Keeping
normalization here isolates the native UI from YouTube Music's shifting response
shapes and from Python-specific authentication details.
"""

from __future__ import annotations

import argparse
import ast
import json
import os
import re
import sys
import traceback
from pathlib import Path
from typing import Any, Iterable


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--auth")
    parser.add_argument("--language", default="ko")
    parser.add_argument("--location", default="KR")
    return parser.parse_args()


def text(value: Any) -> str:
    return "" if value is None else str(value)


def parse_duration(value: Any) -> int | None:
    if isinstance(value, (int, float)):
        return int(value)
    if not isinstance(value, str) or not value:
        return None
    try:
        parts = [int(part) for part in value.split(":")]
    except ValueError:
        return None
    total = 0
    for part in parts:
        total = total * 60 + part
    return total


def extract_braced_object(source: str, start: int) -> str:
    depth = 0
    quote: str | None = None
    escaped = False
    for index in range(start, len(source)):
        char = source[index]
        if quote is not None:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == quote:
                quote = None
            continue
        if char in ('"', "'", "`"):
            quote = char
        elif char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                return source[start : index + 1]
    raise ValueError("fetch 코드에서 headers 객체의 끝을 찾지 못했습니다.")


def normalize_auth_input(raw: str) -> str:
    """Convert Chrome Copy as fetch (Node.js) output to ytmusicapi headers."""
    source = raw.strip()
    if not source:
        return source
    if not source.startswith("fetch(") and not source.startswith("{"):
        return source

    if source.startswith("fetch("):
        url_match = re.match(r"fetch\(\s*(['\"])(.*?)\1\s*,", source, re.DOTALL)
        if not url_match or "music.youtube.com/" not in url_match.group(2):
            raise ValueError("music.youtube.com의 fetch 요청을 붙여 넣어 주세요.")
        headers_match = re.search(r"(?:[\"']headers[\"']|\bheaders)\s*:\s*{", source)
        if not headers_match:
            raise ValueError("fetch 코드에서 headers 객체를 찾지 못했습니다.")
        object_start = source.find("{", headers_match.start())
        encoded = extract_braced_object(source, object_start)
    else:
        encoded = source

    try:
        parsed = json.loads(encoded)
    except json.JSONDecodeError:
        try:
            parsed = ast.literal_eval(encoded)
        except (SyntaxError, ValueError) as exc:
            raise ValueError("Copy as fetch (Node.js) 코드를 해석하지 못했습니다.") from exc

    if isinstance(parsed, dict) and isinstance(parsed.get("headers"), dict):
        parsed = parsed["headers"]
    if not isinstance(parsed, dict):
        raise ValueError("fetch 코드의 headers가 객체 형식이 아닙니다.")

    lines: list[str] = []
    for key, value in parsed.items():
        key = text(key).strip()
        value = text(value).strip()
        if not key or any(char in key + value for char in "\r\n"):
            continue
        lines.append(f"{key}: {value}")
    return "\n".join(lines)


def thumbnail(raw: Any) -> str | None:
    thumbnails = raw.get("thumbnails") if isinstance(raw, dict) else None
    if not thumbnails:
        return None
    candidates = [item for item in thumbnails if isinstance(item, dict) and item.get("url")]
    if not candidates:
        return None
    # The largest candidate is often far larger than any artwork view and stays
    # resident as a decoded texture. Keep enough pixels for a Retina 190 pt card
    # while avoiding multi-megapixel thumbnails.
    candidates.sort(key=lambda item: item.get("width", 0) * item.get("height", 0))
    suitable = [
        item
        for item in candidates
        if item.get("width", 0) >= 384 and item.get("height", 0) >= 384
    ]
    return (suitable[0] if suitable else candidates[-1])["url"]


def item_thumbnail(raw: Any, video_id: str | None) -> str | None:
    resolved = thumbnail(raw)
    if resolved or not video_id or not re.fullmatch(r"[A-Za-z0-9_-]+", video_id):
        return resolved
    # Playlist/watch responses often omit the thumbnail array for tracks even
    # though their video id is present. The stable YouTube thumbnail endpoint
    # avoids rendering a list of anonymous music-note placeholders.
    return f"https://i.ytimg.com/vi/{video_id}/hqdefault.jpg"


def names(values: Any) -> list[str]:
    if not isinstance(values, list):
        return []
    result: list[str] = []
    for item in values:
        if isinstance(item, dict):
            value = item.get("name") or item.get("title")
        else:
            value = item
        if value:
            result.append(str(value))
    return result


def infer_kind(raw: dict[str, Any], fallback: str = "unknown") -> str:
    kind = raw.get("resultType") or raw.get("type") or fallback
    if kind == "profile":
        return "artist"
    browse_id = text(raw.get("browseId"))
    playlist_id = text(raw.get("playlistId"))
    # Explore's new-release cards sometimes omit resultType and expose an album
    # as a watch-playlist id such as VLMPRE.... Treating that id as a regular
    # playlist makes ytmusicapi parse an album response with the playlist schema.
    if browse_id.startswith(("MPRE", "VLMPRE")) or playlist_id.startswith(("MPRE", "VLMPRE")):
        return "album"
    if raw.get("videoId") and kind in ("unknown", "watch"):
        return "song"
    if raw.get("playlistId") and kind == "unknown":
        return "playlist"
    return str(kind).lower()


def normalize_item(raw: Any, fallback_kind: str = "unknown") -> dict[str, Any] | None:
    if not isinstance(raw, dict):
        return None
    kind = infer_kind(raw, fallback_kind)
    video_id = raw.get("videoId")
    browse_id = raw.get("browseId")
    playlist_id = raw.get("playlistId")
    if kind in ("album", "single"):
        album_id = text(browse_id or playlist_id)
        if album_id.startswith("VLMPRE"):
            album_id = album_id[2:]
        if album_id.startswith("MPRE"):
            browse_id = album_id
    if not browse_id:
        browse_id = raw.get("channelId")
    artist_values = raw.get("artists") if isinstance(raw.get("artists"), list) else []
    if kind == "artist" and not browse_id and artist_values:
        browse_id = artist_values[0].get("id")
    if kind == "playlist" and not playlist_id:
        playlist_id = browse_id

    artist_names = names(artist_values)
    subtitle_parts: list[str] = []
    subtitle_parts.extend(artist_names)
    for key in ("album", "year", "category"):
        value = raw.get(key)
        if isinstance(value, dict):
            value = value.get("name")
        if value and str(value) not in subtitle_parts:
            subtitle_parts.append(str(value))
    if not subtitle_parts:
        subtitle_parts.extend(names(raw.get("authors")))
    author = raw.get("author")
    if author and not isinstance(author, (dict, list)) and str(author) not in subtitle_parts:
        subtitle_parts.append(str(author))
    if not subtitle_parts and raw.get("description"):
        subtitle_parts.append(str(raw["description"]).replace("\n", " ")[:120])

    title = raw.get("title") or raw.get("name") or raw.get("artist")
    if not title and kind == "artist" and artist_names:
        title = artist_names[0]
    item_id = video_id or browse_id or playlist_id or raw.get("id") or title or ""
    return {
        "id": text(item_id),
        "kind": kind,
        "title": text(title or "제목 없음"),
        "subtitle": " · ".join(subtitle_parts),
        "videoId": video_id,
        "browseId": browse_id,
        "playlistId": playlist_id,
        "thumbnail": item_thumbnail(raw, video_id),
        "durationSeconds": parse_duration(
            raw.get("duration_seconds") or raw.get("duration") or raw.get("length")
        ),
        "explicit": bool(raw.get("isExplicit") or raw.get("explicit")),
    }


def normalize_items(values: Any, fallback_kind: str = "unknown") -> list[dict[str, Any]]:
    if not isinstance(values, list):
        return []
    return [item for raw in values if (item := normalize_item(raw, fallback_kind)) is not None]


def normalize_sections(values: Any) -> list[dict[str, Any]]:
    if isinstance(values, dict):
        values = [values]
    if not isinstance(values, list):
        return []
    result: list[dict[str, Any]] = []
    for section in values:
        if not isinstance(section, dict):
            continue
        contents = section.get("contents") or section.get("items") or section.get("results")
        items = normalize_items(contents)
        if items:
            result.append(
                {
                    "title": text(section.get("title") or section.get("name") or "추천"),
                    "subtitle": text(section.get("subtitle")),
                    "items": items,
                }
            )
    return result


def detail_page(raw: dict[str, Any], kind: str) -> dict[str, Any]:
    sections: list[dict[str, Any]] = []
    tracks = raw.get("tracks") or raw.get("songs")
    if tracks:
        sections.append({"title": "노래", "subtitle": "", "items": normalize_items(tracks, "song")})

    for key, title in (
        ("albums", "앨범"),
        ("singles", "싱글"),
        ("videos", "동영상"),
        ("related", "관련 콘텐츠"),
        ("playlists", "플레이리스트"),
    ):
        value = raw.get(key)
        if isinstance(value, dict):
            value = value.get("results") or value.get("contents")
        items = normalize_items(value, key.rstrip("s"))
        if items:
            sections.append({"title": title, "subtitle": "", "items": items})

    author_names = names(raw.get("artists") or raw.get("author"))
    subtitle_parts = author_names + [text(raw.get("year"))] if raw.get("year") else author_names
    return {
        "title": text(raw.get("title") or raw.get("name") or kind.title()),
        "subtitle": " · ".join(part for part in subtitle_parts if part),
        "description": text(raw.get("description")),
        "thumbnail": thumbnail(raw),
        "sections": sections,
    }


class Service:
    def __init__(self, args: argparse.Namespace) -> None:
        try:
            from ytmusicapi import YTMusic
        except ImportError as exc:
            raise RuntimeError(
                "ytmusicapi가 설치되지 않았습니다. `./scripts/bootstrap.sh`를 먼저 실행하세요."
            ) from exc
        self.auth_path = Path(args.auth).expanduser() if args.auth else None
        self.account_path = (
            self.auth_path.with_name("account.json") if self.auth_path else None
        )
        self.language = args.language
        self.location = args.location
        auth = str(self.auth_path) if self.auth_path and self.auth_path.is_file() else None
        self.authenticated = bool(auth)
        try:
            self.yt = YTMusic(auth=auth, language=self.language, location=self.location)
        except Exception:
            self.authenticated = False
            self.yt = YTMusic(language=self.language, location=self.location)
        self.account = self.read_account_cache() if self.authenticated else {}

    def read_account_cache(self) -> dict[str, Any]:
        if self.account_path is None or not self.account_path.is_file():
            return {}
        try:
            raw = json.loads(self.account_path.read_text(encoding="utf-8"))
            return raw if isinstance(raw, dict) else {}
        except (OSError, json.JSONDecodeError):
            return {}

    def write_account_cache(self) -> None:
        if self.account_path is None or not self.account:
            return
        cached = {
            key: self.account[key]
            for key in ("accountName", "name", "channelHandle", "handle", "accountPhotoUrl", "thumbnail")
            if self.account.get(key)
        }
        if not cached:
            return
        self.account_path.parent.mkdir(parents=True, exist_ok=True)
        temporary = self.account_path.with_suffix(self.account_path.suffix + ".tmp")
        temporary.write_text(json.dumps(cached, ensure_ascii=False), encoding="utf-8")
        os.replace(temporary, self.account_path)
        if os.name != "nt":
            os.chmod(self.account_path, 0o600)

    def account_status(self) -> dict[str, Any]:
        raw = self.account if self.authenticated else {}
        return {
            "authenticated": self.authenticated,
            "name": text(raw.get("accountName") or raw.get("name")),
            "handle": text(raw.get("channelHandle") or raw.get("handle")),
            "thumbnail": raw.get("accountPhotoUrl") or raw.get("thumbnail"),
        }

    def authenticate(self, headers_raw: str) -> dict[str, Any]:
        if not headers_raw.strip():
            raise ValueError("복사한 요청 헤더를 입력하세요.")
        if self.auth_path is None:
            raise RuntimeError("인증 파일 저장 경로가 설정되지 않았습니다.")

        from ytmusicapi import YTMusic, setup

        auth_json = setup(headers_raw=normalize_auth_input(headers_raw))
        parsed_headers = json.loads(auth_json)
        missing = {"authorization", "cookie"} - {key.lower() for key in parsed_headers}
        if missing:
            raise ValueError(
                "인증에 필요한 헤더가 없습니다: " + ", ".join(sorted(missing))
            )
        authenticated = YTMusic(
            auth=auth_json,
            language=self.language,
            location=self.location,
        )
        # Validate against a library endpoint. Account-menu can intermittently return
        # an empty body for otherwise valid browser sessions.
        try:
            authenticated.get_library_playlists(limit=1)
        except Exception as exc:
            raise RuntimeError(
                "세션을 확인하지 못했습니다. 로그인된 music.youtube.com의 /browse POST 요청 헤더를 다시 복사하세요."
            ) from exc
        try:
            account = authenticated.get_account_info()
        except Exception:
            account = {}
        self.auth_path.parent.mkdir(parents=True, exist_ok=True)
        temporary = self.auth_path.with_suffix(self.auth_path.suffix + ".tmp")
        temporary.write_text(auth_json, encoding="utf-8")
        os.replace(temporary, self.auth_path)
        self.write_playback_cookies(auth_json)
        if os.name != "nt":
            os.chmod(self.auth_path, 0o600)
        self.yt = authenticated
        self.authenticated = True
        self.account = account if isinstance(account, dict) else {}
        if self.account_path is not None:
            self.account_path.unlink(missing_ok=True)
        self.write_account_cache()
        return self.account_status()

    def logout(self) -> dict[str, Any]:
        from ytmusicapi import YTMusic

        if self.auth_path is not None:
            self.auth_path.unlink(missing_ok=True)
            self.auth_path.with_name("cookies.txt").unlink(missing_ok=True)
        if self.account_path is not None:
            self.account_path.unlink(missing_ok=True)
        self.yt = YTMusic(language=self.language, location=self.location)
        self.authenticated = False
        self.account = {}
        return self.account_status()

    def write_playback_cookies(self, auth_json: str) -> None:
        if self.auth_path is None:
            return
        headers = json.loads(auth_json)
        cookie_header = text(headers.get("cookie"))
        if not cookie_header:
            return
        lines = ["# Netscape HTTP Cookie File"]
        for raw_cookie in cookie_header.split(";"):
            name, separator, value = raw_cookie.strip().partition("=")
            if not separator or not name or any(char in name + value for char in "\r\n\t"):
                continue
            lines.append(f".youtube.com\tTRUE\t/\tTRUE\t0\t{name}\t{value}")
        cookie_path = self.auth_path.with_name("cookies.txt")
        temporary = cookie_path.with_suffix(cookie_path.suffix + ".tmp")
        temporary.write_text("\n".join(lines) + "\n", encoding="utf-8")
        os.replace(temporary, cookie_path)
        if os.name != "nt":
            os.chmod(cookie_path, 0o600)

    def dispatch(self, op: str, params: dict[str, Any]) -> Any:
        if op in ("ping", "authStatus"):
            return self.account_status()
        if op == "quickLogin":
            from chrome_login import capture_browser_auth_headers

            return self.authenticate(capture_browser_auth_headers())
        if op == "authenticate":
            return self.authenticate(text(params.get("headers")))
        if op == "logout":
            return self.logout()
        if op == "home":
            sections = normalize_sections(self.yt.get_home(limit=int(params.get("limit", 8))))
            return sections or self.fallback_home()
        if op == "explore":
            raw = self.yt.get_explore()
            sections: list[dict[str, Any]] = []
            for key, title in (
                ("new_releases", "새 앨범 및 싱글"),
                ("top_songs", "인기곡"),
                ("moods", "분위기 및 장르"),
                ("trending", "인기 급상승"),
            ):
                value = raw.get(key) if isinstance(raw, dict) else None
                if isinstance(value, dict):
                    value = value.get("items") or value.get("contents") or value.get("results")
                items = normalize_items(value)
                if items:
                    sections.append({"title": title, "subtitle": "", "items": items})
            return sections or normalize_sections(raw) or self.fallback_explore()
        if op == "search":
            return normalize_items(
                self.yt.search(text(params.get("query")), limit=int(params.get("limit", 40)))
            )
        if op == "library":
            self.require_auth()
            limit = int(params.get("limit", 100))
            category = params.get("category", "all")
            sections: list[dict[str, Any]] = []
            if category in ("all", "playlists"):
                sections.append({"title": "플레이리스트", "subtitle": "", "items": normalize_items(self.yt.get_library_playlists(limit=limit), "playlist")})
            if category in ("all", "songs"):
                sections.append({"title": "보관함의 노래", "subtitle": "", "items": normalize_items(self.yt.get_library_songs(limit=limit), "song")})
            if category in ("all", "albums"):
                sections.append({"title": "앨범", "subtitle": "", "items": normalize_items(self.yt.get_library_albums(limit=limit), "album")})
            if category in ("all", "artists"):
                sections.append({"title": "아티스트", "subtitle": "", "items": normalize_items(self.yt.get_library_artists(limit=limit), "artist")})
            return [section for section in sections if section["items"]]
        if op == "watch":
            raw = self.yt.get_watch_playlist(
                videoId=params["videoId"], limit=int(params.get("limit", 50))
            )
            return {
                "playlistId": raw.get("playlistId"),
                "lyricsBrowseId": raw.get("lyrics"),
                "items": normalize_items(raw.get("tracks"), "song"),
            }
        if op == "playlistQueue":
            self.require_auth()
            raw = self.yt.get_watch_playlist(
                playlistId=params["playlistId"], limit=int(params.get("limit", 50))
            )
            return {
                "playlistId": raw.get("playlistId") or params["playlistId"],
                "lyricsBrowseId": raw.get("lyrics"),
                "items": normalize_items(raw.get("tracks"), "song"),
            }
        if op == "browse":
            kind = text(params.get("kind")).lower()
            browse_id = params.get("browseId")
            playlist_id = params.get("playlistId")
            if kind == "artist":
                raw = self.yt.get_artist(browse_id)
            elif kind in ("album", "single"):
                raw = self.yt.get_album(browse_id)
            else:
                raw = self.yt.get_playlist(playlist_id or browse_id, limit=100)
            return detail_page(raw, kind)
        if op == "lyrics":
            raw = self.yt.get_lyrics(params["browseId"], timestamps=False) or {}
            if hasattr(raw, "model_dump"):
                raw = raw.model_dump()
            return {"source": text(raw.get("source")), "text": text(raw.get("lyrics"))}
        if op == "rateSong":
            self.require_auth()
            return self.yt.rate_song(params["videoId"], params.get("rating", "LIKE"))
        if op == "createPlaylist":
            self.require_auth()
            return self.yt.create_playlist(
                params["title"],
                params.get("description", ""),
                params.get("privacyStatus", "PRIVATE"),
                params.get("videoIds"),
            )
        if op == "addPlaylistItems":
            self.require_auth()
            return self.yt.add_playlist_items(
                params["playlistId"], params["videoIds"], duplicates=params.get("duplicates", False)
            )
        raise ValueError(f"지원하지 않는 operation: {op}")

    def require_auth(self) -> None:
        if not self.authenticated:
            raise PermissionError(
                "이 기능은 로그인이 필요합니다. ytmusicapi browser 인증 파일을 auth.json으로 저장하세요."
            )

    def fallback_home(self) -> list[dict[str, Any]]:
        """Keep unauthenticated mode useful when regional home feeds are gated."""
        result: list[dict[str, Any]] = []
        for query, title in (("인기 음악", "지금 인기 있는 음악"), ("편안한 음악", "편안한 시간")):
            items = normalize_items(self.yt.search(query, limit=12))[:12]
            if items:
                result.append({"title": title, "subtitle": "", "items": items})
        return result

    def fallback_explore(self) -> list[dict[str, Any]]:
        result: list[dict[str, Any]] = []
        for query, title, filter_name in (
            ("새 음악", "새 앨범 및 싱글", "albums"),
            ("운동 음악", "운동할 때 듣는 음악", "playlists"),
            ("집중 음악", "집중을 위한 음악", "songs"),
        ):
            items = normalize_items(self.yt.search(query, filter=filter_name, limit=12))
            if not items:
                items = normalize_items(self.yt.search(query, limit=12))[:12]
            if items:
                result.append({"title": title, "subtitle": "", "items": items})
        return result


def write_response(request_id: int, ok: bool, data: Any = None, error: str = "") -> None:
    print(
        json.dumps(
            {"id": request_id, "ok": ok, "data": data, "error": error},
            ensure_ascii=False,
            separators=(",", ":"),
            default=str,
        ),
        flush=True,
    )


def main() -> None:
    args = parse_args()
    service: Service | None = None
    startup_error: Exception | None = None
    try:
        service = Service(args)
    except Exception as exc:  # report startup issues through the protocol
        startup_error = exc

    for line in sys.stdin:
        request_id = 0
        try:
            request = json.loads(line)
            request_id = int(request.get("id", 0))
            if startup_error is not None:
                raise startup_error
            assert service is not None
            data = service.dispatch(request["op"], request.get("params") or {})
            write_response(request_id, True, data=data)
        except Exception as exc:
            if "POCKET_YTM_DEBUG" in __import__("os").environ:
                traceback.print_exc(file=sys.stderr)
            write_response(request_id, False, error=f"{type(exc).__name__}: {exc}")


if __name__ == "__main__":
    main()
