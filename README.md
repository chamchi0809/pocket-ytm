# Pocket Music

<img src="assets/pocket-music-library.png" alt="Pocket Music 보관함 화면" width="512">

WebView, Chromium 없이 Rust로 구현된 네이티브 YouTube Music 클라이언트입니다.

- YouTube Music Premium 계정이 없어도 광고 없이 재생할 수 있습니다. 단, 비로그인 상태에서는 YouTube 정책에 따라 일부 영상 재생이 제한될 수 있습니다.
- Chromium을 띄우지 않아 YouTube Music PWA보다 RAM을 적게 사용합니다.

## 설치

현재 배포판은 Apple Silicon Mac용입니다.

1. [최신 릴리스](https://github.com/chamchi0809/pocket-ytm/releases/latest)에서 `Pocket-Music-*-macos-aarch64.zip`을 받습니다.
2. 압축을 풀고 `Pocket Music.app`을 Applications 폴더로 옮깁니다.
3. 앱을 실행합니다.

macOS가 첫 실행을 막으면 Finder에서 앱을 우클릭해 `열기`를 선택하세요. 그래도 차단되면 `시스템 설정 → 개인정보 보호 및 보안`에서 `확인 없이 열기`를 누르면 됩니다.

앱은 시작할 때 새 릴리스를 확인합니다. 업데이트해도 로그인, 쿠키, 볼륨 설정은 그대로 유지됩니다.

## 기능

- 홈 추천, 둘러보기, 검색
- 앨범, 아티스트, 플레이리스트 상세 화면과 보관함
- 백그라운드 재생, seek, 이전·다음 곡, 셔플, 반복, 볼륨 저장
- 세션 트랙 캐시와 다음·이전 곡 prefetch
- YouTube Music 라디오 큐, 가사, 좋아요
- GitHub Releases 기반 자동 업데이트

## 로그인

로그인 없이도 홈, 둘러보기, 검색을 사용할 수 있으며 재생 가능한 영상은 광고 없이 들을 수 있습니다. 다만 비로그인 재생은 YouTube의 제한을 받을 수 있습니다.
앱의 YouTube Music 로그인 기능은 모든 유튜브 계정을 지원하며, 로그인하면 보관함과 좋아요를 사용할 수 있습니다.

`Google로 빠르게 로그인`을 누르면 자동화 옵션 없는 Pocket Music 전용 Chrome 창이 열립니다. Google 로그인이 완료되면 앱이 이를 감지하고, 같은 임시 프로필의 로그인 쿠키로 자동 연결한 뒤 프로필을 삭제합니다. 완료 후에도 계속 기다린다면 로그인 창을 완전히 닫으면 즉시 다음 단계로 넘어갑니다. Chrome, Edge, Brave 또는 Chromium을 찾지 못하면 아래 수동 방식을 사용할 수 있습니다.

1. `music.youtube.com 열기`를 누르고 브라우저에서 로그인합니다.
2. 개발자 도구를 열고 Network 탭을 선택합니다.
3. 개발자 도구를 연 상태로 YouTube Music의 `보관함`으로 이동합니다. Network 탭에 `/browse` POST 요청이 생성됩니다.
4. 해당 요청을 우클릭하고 `Copy → Copy as fetch (Node.js)`를 선택합니다.
5. 복사한 fetch 코드 전체를 앱에 붙여 넣습니다.

앱은 fetch 코드를 실행하지 않고 요청 헤더만 읽습니다. 인증 정보는 macOS 사용자 설정 폴더에 저장되며 저장소에 포함되지 않습니다.

## 기여자를 위한 개발 안내

Pocket Music은 GPUI로 화면을 그리고 rodio/CPAL로 오디오를 출력합니다. ytmusicapi와 상주 yt-dlp resolver는 Rust 프로세스와 NDJSON으로 통신합니다. Rust는 오디오를 HTTP Range 단위로 받아 FFmpeg에 전달하며, 세션 캐시는 앱을 종료할 때 삭제합니다.

### 로컬 실행

최신 stable Rust, Python 3.10 이상, FFmpeg가 필요합니다.

```sh
./scripts/bootstrap.sh
cargo run --release
```

### 테스트와 패키징

```sh
cargo fmt --all -- --check
cargo test
(cd backend && ../.venv/bin/python -m unittest test_ytmusic_bridge.py test_yt_dlp_resolver.py)
./scripts/package-macos.sh
```

패키징 결과는 `dist/Pocket Music.app`에 생성됩니다. Apple Silicon용 ytmusicapi, yt-dlp, Deno, FFmpeg, ffprobe와 업데이트 helper가 모두 앱에 포함됩니다.

### 릴리스

`Cargo.toml` 버전과 같은 태그를 push하면 GitHub Actions가 Apple Silicon 앱과 서명된 업데이트 manifest를 릴리스에 올립니다.

```sh
git tag v0.2.0
git push origin v0.2.0
```

릴리스에는 `UPDATE_SIGNING_KEY_BASE64` Actions secret이 필요합니다. 공개 배포에서 macOS 경고를 없애려면 Developer ID 서명과 notarization도 설정해야 합니다.

### 환경 변수

| 변수                  | 용도                                 |
| --------------------- | ------------------------------------ |
| `POCKET_YTM_AUTH`     | ytmusicapi 인증 파일 경로            |
| `POCKET_YTM_COOKIES`  | 재생용 쿠키 파일 경로                |
| `POCKET_YTM_BRIDGE`   | ytmusicapi 브리지 override           |
| `POCKET_YTM_RESOLVER` | yt-dlp resolver override             |
| `POCKET_YTM_FFMPEG`   | FFmpeg override                      |
| `POCKET_YTM_DENO`     | Deno override                        |
| `POCKET_YTM_BROWSER`  | 빠른 로그인용 Chromium 브라우저 경로 |
| `POCKET_YTM_LANGUAGE` | YouTube Music 언어, 기본값 `ko`      |
| `POCKET_YTM_LOCATION` | YouTube Music 국가, 기본값 `KR`      |

## 호환성

ytmusicapi와 yt-dlp는 YouTube 변경에 따라 업데이트가 필요할 수 있습니다. DRM, Cast, Premium 오프라인 저장, 구매와 결제 기능은 지원하지 않습니다. YouTube 이용약관과 콘텐츠 권리를 준수해 사용하세요.
