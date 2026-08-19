# Pocket Music

GPUI로 직접 그리는 크로스플랫폼 YouTube Music 클라이언트입니다. 화면에는 DOM/WebView가 없고,
Chromium을 번들하지 않습니다. 오디오는 HTML `<audio>` 대신 `yt-dlp`의 stdout을 FFmpeg에서
일관된 PCM/WAV로 정규화하고 seek 가능한 앱 세션 임시 캐시에 청크 단위로 스트리밍한 뒤
rodio/CPAL을 통해 OS 오디오 장치로 보냅니다. 이 캐시는 앱을 종료하면 삭제되며 다음 실행으로
영구 보존되지 않습니다.

## 구현된 코어 기능

- 홈 추천, 둘러보기, 통합 검색
- 앨범·아티스트·플레이리스트 상세 탐색
- 로그인 사용자의 노래·앨범·아티스트·플레이리스트 보관함
- 네이티브 백그라운드 재생, 일시정지, seek, 이전/다음, 볼륨
- 앱 재시작 후에도 복원되는 볼륨 설정
- 현재 세션의 트랙 캐시와 다음·이전 트랙 백그라운드 prefetch
- YouTube Music 라디오 큐, 자동 다음 곡, 셔플, 한 곡/전체 반복
- 가사 패널과 곡 좋아요
- 원격 썸네일의 네이티브 GPUI 렌더링
- 한글/CJK IME 검색 입력
- 시스템 브라우저 세션을 연결하는 네이티브 로그인·계정·로그아웃 UI
- Ed25519 서명을 검증하는 GitHub Releases 기반 네이티브 자동 업데이트

Python 어댑터에는 플레이리스트 생성과 곡 추가 operation도 마련되어 있어 UI 기능을 확장할 때
Rust 쪽에 YouTube 응답 형식을 노출할 필요가 없습니다.

## 구조

```text
GPUI native UI
    ├── bundled ytmusicapi sidecar (stdin/stdout NDJSON) ── 탐색·계정·라이브러리
    └── native audio thread
            └── bundled yt-dlp + Deno ── bundled FFmpeg ── 64KB PCM/WAV chunks
                    └── session seek cache ── rodio/CPAL ── OS audio
```

`gpui-component`의 WebView feature를 비활성화했고 `wry`/Chromium은 의존성에 포함되지 않습니다.

## 빠른 시작

소스에서 개발할 때 필요한 도구는 최신 stable Rust와 Python 3.10+입니다. 시스템 `yt-dlp`와
FFmpeg는 개발 실행의 fallback으로 사용할 수 있습니다.

```sh
./scripts/bootstrap.sh
cargo run --release
```

macOS `.app` 번들은 다음 명령으로 만듭니다. 결과물은 `dist/Pocket Music.app`입니다.

```sh
./scripts/package-macos.sh
```

패키징 스크립트는 Apple Silicon용 ytmusicapi/Python 브리지, 공식 yt-dlp standalone, Deno,
외부 라이브러리 없이 빌드한 FFmpeg/ffprobe와 업데이트 helper를 모두 `.app` 안에 포함합니다.
설치 사용자는 Python이나 Homebrew를 설치할 필요가 없습니다. 새 버전은 기존 `.app`만 원자적으로 교체하므로
`~/Library/Application Support/Pocket Music`의 로그인·쿠키 데이터는 유지됩니다.

## 자동 업데이트와 릴리스

앱 시작 시 공개 GitHub 저장소의 최신 정식 릴리스를 확인합니다. 새 버전이 있으면 GPUI 업데이트
창에서 다운로드하고, 다음 검증을 모두 통과한 경우에만 앱을 교체한 뒤 재시작합니다.

- 앱에 내장된 Ed25519 공개키로 `update-manifest.json.sig` 검증
- manifest 저장소·버전·플랫폼·다운로드 경로 검증
- ZIP 크기와 SHA-256 검증
- 압축 경로 순회 차단과 bundle ID·앱 버전 확인
- macOS 코드 서명 확인

GitHub Actions의 `Release` workflow는 `Cargo.toml` 버전과 같은 태그를 push하면 Apple Silicon 앱,
서명된 manifest, GitHub Release를 자동 생성합니다.

```sh
# Cargo.toml의 version을 먼저 0.2.0으로 변경
git commit -am "Release 0.2.0"
git tag v0.2.0
git push origin main v0.2.0
```

manifest 개인키 seed는 저장소 파일이 아니라 `UPDATE_SIGNING_KEY_BASE64` Actions secret으로만 관리합니다.
macOS 앱 자체는 현재 ad-hoc 코드 서명됩니다. 다른 Mac에 경고 없는 공개 배포를 하려면 Developer ID
Application 서명과 Apple notarization 단계를 추가로 구성해야 합니다.

이 프로젝트는 macOS에서 전체 Xcode 없이도 빌드할 수 있도록 GPUI의 `macos-blade` 렌더러를 켭니다.
표준 GPUI Metal 렌더러로 되돌릴 경우 전체 Xcode를 설치하고 아래처럼 developer directory를 선택해야
합니다.

```sh
sudo xcode-select --switch /Applications/Xcode.app/Contents/Developer
```

Linux에서는 배포판에 맞는 ALSA, fontconfig, Vulkan, X11/Wayland 개발 패키지가 필요합니다. Windows는
Rust MSVC toolchain과 WebView가 아닌 네이티브 GPUI/CPAL 경로를 사용합니다.

## 로그인과 쿠키

로그인 없이도 홈·둘러보기·검색·재생을 사용할 수 있습니다. 보관함과 좋아요는 헤더의 `로그인`
버튼에서 연결할 수 있습니다.

1. 로그인 패널에서 `music.youtube.com 열기`를 누르고 시스템 브라우저에서 로그인합니다.
2. 브라우저 개발자 도구의 Network 탭에서 `/browse` POST 요청을 우클릭합니다.
3. `Copy` → `Copy as fetch (Node.js)`를 선택하고, 복사된 코드 전체를 앱에 붙여 넣습니다.

앱은 fetch 코드를 실행하지 않고 `headers` 객체만 파싱합니다. 헤더를 검증한 뒤 플랫폼의 사용자 설정
폴더에 `auth.json`을 권한 `0600`으로 저장합니다. Cookie
헤더는 yt-dlp 재생에 필요한 Netscape `cookies.txt`로 함께 변환됩니다. 내장 브라우저나 DOM은 사용하지
않습니다.

터미널에서 인증 파일을 직접 만들 수도 있습니다.

```sh
.venv/bin/ytmusicapi browser --file auth.json
```

화면 지시에 따라 `music.youtube.com`의 로그인된 `/browse` 요청 헤더를 붙여 넣으세요. 다른 위치에
저장했다면 `POCKET_YTM_AUTH=/absolute/path/browser.json`으로 지정할 수 있습니다.

연령 제한 또는 계정 전용 트랙 재생에는 yt-dlp용 Netscape 형식 쿠키가 별도로 필요할 수 있습니다.
프로젝트 루트의 `cookies.txt`를 자동 인식하며, 다른 경로는
`POCKET_YTM_COOKIES=/absolute/path/cookies.txt`로 지정합니다. 인증 파일과 쿠키는 `.gitignore`에
포함되어 있으며 커밋하면 안 됩니다.

## 환경 변수

| 변수 | 기본값 | 용도 |
|---|---|---|
| `POCKET_YTM_PYTHON` | `.venv` 또는 `python3` | 소스 실행용 Python fallback |
| `POCKET_YTM_BRIDGE` | 앱 내장 브리지 | ytmusicapi 실행 파일 또는 Python 스크립트 |
| `POCKET_YTM_AUTH` | 사용자 설정 폴더의 `auth.json` | ytmusicapi browser 인증 |
| `POCKET_YTM_SETTINGS` | 인증 파일 옆 `settings.json` | 볼륨 등 영속 설정 |
| `POCKET_YTM_YTDLP` | 앱 내장 `yt-dlp` | yt-dlp 실행 파일 override |
| `POCKET_YTM_FFMPEG` | 앱 내장 `ffmpeg` | 오디오 정규화용 FFmpeg override |
| `POCKET_YTM_DENO` | 앱 내장 `deno` | yt-dlp YouTube JS challenge runtime override |
| `POCKET_YTM_COOKIES` | 인증 파일 옆 `cookies.txt` | 재생용 쿠키 |
| `POCKET_YTM_LANGUAGE` | `ko` | YouTube Music 언어 |
| `POCKET_YTM_LOCATION` | `KR` | YouTube Music 국가 |
| `POCKET_YTM_DEBUG` | 미설정 | Python traceback 출력 |

## 단축키

- `Cmd+K` (`Ctrl+K`): 검색창 포커스
- `Space`: 재생/일시정지
- `Cmd+←` / `Cmd+→` (`Ctrl` on Linux/Windows): 이전/다음 곡
- `Cmd+Q` (`Ctrl+Q`): 종료

화면의 모든 플레이어 컨트롤은 마우스로도 사용할 수 있습니다.

## 검증

```sh
cargo fmt --all -- --check
cargo test
(cd backend && ../.venv/bin/python -m unittest test_ytmusic_bridge.py)
./scripts/build-macos-dependencies.sh
./scripts/package-macos.sh
```

## 현실적인 호환성 경계

`ytmusicapi`와 `yt-dlp`는 YouTube의 비공개/역공학 인터페이스를 사용하므로 YouTube 변경에 따라
업데이트가 필요할 수 있습니다. 공식 웹 앱과 픽셀 단위로 같거나 DRM, Cast, 오프라인 Premium 저장,
구매/결제를 복제하는 앱은 아닙니다. 이 구현은 음악 탐색·라이브러리·큐·가사·좋아요·네이티브 재생이라는
코어 흐름을 재현하며, 사용자는 YouTube 이용약관과 콘텐츠 권리를 준수해야 합니다.
