# Pocket Music bundled dependencies

The macOS application includes the following runtime components so users do not
need Python, Homebrew, yt-dlp, FFmpeg, or a JavaScript runtime installed.

- ytmusicapi 1.12.2 — MIT
- CPython 3.12 runtime and Python packages collected by PyInstaller 6.22.2
- yt-dlp 2026.07.04 embedded in the persistent PyInstaller resolver — Unlicense;
  optional bundled components retain their respective upstream licenses
- FFmpeg 9.0.1 built without GPL or external libraries — LGPL-2.1-or-later
- Deno 2.9.5 — MIT

The corresponding upstream license files are included beside this notice.
Project sources:

- https://github.com/sigma67/ytmusicapi
- https://github.com/pyinstaller/pyinstaller
- https://github.com/yt-dlp/yt-dlp
- https://ffmpeg.org/
- https://github.com/denoland/deno
