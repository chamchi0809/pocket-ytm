use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::{BufReader, Read as _, Write as _},
    path::{Path, PathBuf},
    process::{Command as StdCommand, Stdio},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use parking_lot::RwLock;
use rodio::{
    Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source,
    cpal::{self, traits::DeviceTrait as _, traits::HostTrait as _},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use stream_download::{
    Settings, StreamDownload,
    process::{Command as StreamCommand, ProcessStreamParams},
    storage::temp::TempStorageProvider,
};

use crate::{config::AppConfig, model::MediaItem};

const MEDIA_STREAM_FLAG: &str = "--pocket-music-media-stream";
const SESSION_CACHE_PREFIX: &str = "pocket-music-session-";
const STREAM_PREFETCH_BYTES: u64 = 1024 * 128;
const PREFETCH_START_DELAY: Duration = Duration::from_secs(1);
const OUTPUT_RETRY_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackPhase {
    Idle,
    Loading,
    Playing,
    Paused,
    Ended,
    Error,
}

#[derive(Debug, Clone)]
pub struct AudioSnapshot {
    pub phase: PlaybackPhase,
    pub item: Option<MediaItem>,
    pub position: Duration,
    pub duration: Duration,
    pub volume: f32,
    pub generation: u64,
    pub error: Option<String>,
}

impl Default for AudioSnapshot {
    fn default() -> Self {
        Self {
            phase: PlaybackPhase::Idle,
            item: None,
            position: Duration::ZERO,
            duration: Duration::ZERO,
            volume: 0.8,
            generation: 0,
            error: None,
        }
    }
}

#[derive(Clone)]
pub struct AudioEngine {
    tx: mpsc::Sender<AudioCommand>,
    snapshot: Arc<RwLock<AudioSnapshot>>,
    // The cache directory lives exactly as long as the running app session. TempDir
    // removes every downloaded track after the final engine/worker is dropped.
    _session_cache: Arc<tempfile::TempDir>,
}

enum AudioCommand {
    Load(MediaItem),
    Prefetch(Vec<MediaItem>),
    Toggle,
    Seek(Duration),
    SetVolume(f32),
}

struct LoadRequest {
    generation: u64,
    item: MediaItem,
}

struct PreparedSource {
    generation: u64,
    result: Result<(NativeSource, Option<Duration>), String>,
}

struct AudioOutput {
    _sink: MixerDeviceSink,
    player: Player,
}

type NativeSource = Box<dyn Source + Send>;

impl AudioEngine {
    pub fn new(config: AppConfig) -> Self {
        purge_session_cache_dirs(&std::env::temp_dir());
        let session_cache = Arc::new(
            tempfile::Builder::new()
                .prefix(SESSION_CACHE_PREFIX)
                .tempdir()
                .expect("failed to create session audio cache"),
        );
        let (tx, rx) = mpsc::channel();
        let initial_snapshot = AudioSnapshot {
            volume: load_saved_volume(&config.settings_path).unwrap_or(0.8),
            ..Default::default()
        };
        let snapshot = Arc::new(RwLock::new(initial_snapshot));
        let thread_snapshot = snapshot.clone();
        let thread_cache = session_cache.clone();
        thread::Builder::new()
            .name("pocket-ytm-audio".into())
            .spawn(move || audio_loop(config, thread_cache, rx, thread_snapshot))
            .expect("failed to start native audio thread");
        Self {
            tx,
            snapshot,
            _session_cache: session_cache,
        }
    }

    pub fn snapshot(&self) -> AudioSnapshot {
        self.snapshot.read().clone()
    }

    pub fn load(&self, item: MediaItem) {
        let _ = self.tx.send(AudioCommand::Load(item));
    }

    pub fn prefetch(&self, items: Vec<MediaItem>) {
        let _ = self.tx.send(AudioCommand::Prefetch(items));
    }

    pub fn toggle(&self) {
        let _ = self.tx.send(AudioCommand::Toggle);
    }

    pub fn seek(&self, position: Duration) {
        let _ = self.tx.send(AudioCommand::Seek(position));
    }

    pub fn set_volume(&self, volume: f32) {
        let _ = self
            .tx
            .send(AudioCommand::SetVolume(volume.clamp(0.0, 1.0)));
    }
}

fn purge_session_cache_dirs(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_owned_cache = name.starts_with(SESSION_CACHE_PREFIX)
            && entry.file_type().is_ok_and(|kind| kind.is_dir());
        if is_owned_cache && let Err(error) = std::fs::remove_dir_all(entry.path()) {
            log::debug!("stale session cache could not be removed: {error}");
        }
    }
}

fn audio_loop(
    config: AppConfig,
    session_cache: Arc<tempfile::TempDir>,
    rx: mpsc::Receiver<AudioCommand>,
    snapshot: Arc<RwLock<AudioSnapshot>>,
) {
    let mut output = match open_audio_output(snapshot.read().volume) {
        Ok(output) => Some(output),
        Err(error) => {
            log::warn!("initial audio output unavailable; playback will retry: {error:#}");
            None
        }
    };

    let (load_tx, load_rx) = mpsc::channel();
    let (prepared_tx, prepared_rx) = mpsc::channel();
    let loader_config = config.clone();
    let loader_cache = session_cache.clone();
    thread::Builder::new()
        .name("pocket-ytm-source-loader".into())
        .spawn(move || source_loader_loop(loader_config, loader_cache, load_rx, prepared_tx))
        .expect("failed to start source loader");

    let (prefetch_tx, prefetch_rx) = mpsc::channel();
    let prefetch_config = config.clone();
    let prefetch_cache = session_cache.clone();
    thread::Builder::new()
        .name("pocket-ytm-prefetch".into())
        .spawn(move || prefetch_loop(prefetch_config, prefetch_cache, prefetch_rx))
        .expect("failed to start source prefetcher");

    let mut empty_since = None;
    let mut suppress_empty_until = Instant::now();
    let mut pending_load: Option<LoadRequest> = None;
    let mut next_output_retry = Instant::now();

    loop {
        if output.is_none() && pending_load.is_some() && Instant::now() >= next_output_retry {
            match open_audio_output(snapshot.read().volume) {
                Ok(new_output) => {
                    output = Some(new_output);
                    let request = pending_load.take().expect("pending load checked above");
                    if load_tx.send(request).is_err() {
                        break;
                    }
                    let mut state = snapshot.write();
                    state.phase = PlaybackPhase::Loading;
                    state.error = None;
                }
                Err(error) => {
                    set_error(
                        &snapshot,
                        format!("오디오 출력 장치를 열 수 없습니다. 다시 시도 중입니다: {error:#}"),
                    );
                    next_output_retry = Instant::now() + OUTPUT_RETRY_INTERVAL;
                }
            }
        }

        if let Some(output) = &output
            && apply_prepared_sources(&prepared_rx, &output.player, &snapshot)
        {
            empty_since = None;
            suppress_empty_until = Instant::now() + Duration::from_secs(1);
        }

        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(AudioCommand::Load(item)) => {
                if let Some(output) = &output {
                    output.player.stop();
                }
                empty_since = None;
                suppress_empty_until = Instant::now() + Duration::from_secs(1);
                let generation = {
                    let mut state = snapshot.write();
                    state.generation += 1;
                    state.phase = PlaybackPhase::Loading;
                    state.position = Duration::ZERO;
                    state.duration = item
                        .duration_seconds
                        .map(Duration::from_secs)
                        .unwrap_or_default();
                    state.item = Some(item.clone());
                    state.error = None;
                    state.generation
                };
                let request = LoadRequest { generation, item };
                if output.is_some() {
                    let _ = load_tx.send(request);
                    pending_load = None;
                } else {
                    pending_load = Some(request);
                    next_output_retry = Instant::now();
                }
            }
            Ok(AudioCommand::Prefetch(items)) => {
                let _ = prefetch_tx.send(items);
            }
            Ok(AudioCommand::Toggle) => {
                let mut state = snapshot.write();
                if let Some(output) = &output {
                    match state.phase {
                        PlaybackPhase::Playing => {
                            output.player.pause();
                            state.phase = PlaybackPhase::Paused;
                        }
                        PlaybackPhase::Paused => {
                            output.player.play();
                            state.phase = PlaybackPhase::Playing;
                        }
                        _ => {}
                    }
                }
            }
            Ok(AudioCommand::Seek(position)) => {
                if let Some(output) = &output {
                    if let Err(error) = output.player.try_seek(position) {
                        snapshot.write().error = Some(format!("탐색할 수 없습니다: {error}"));
                    } else {
                        empty_since = None;
                        suppress_empty_until = Instant::now() + Duration::from_secs(2);
                        let mut state = snapshot.write();
                        state.position = position;
                        state.error = None;
                    }
                }
            }
            Ok(AudioCommand::SetVolume(volume)) => {
                if let Some(output) = &output {
                    output.player.set_volume(volume);
                }
                snapshot.write().volume = volume;
                if let Err(error) = save_volume(&config.settings_path, volume) {
                    log::warn!("볼륨 설정을 저장하지 못했습니다: {error:#}");
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        if let Some(output) = &output {
            if apply_prepared_sources(&prepared_rx, &output.player, &snapshot) {
                empty_since = None;
                suppress_empty_until = Instant::now() + Duration::from_secs(1);
            }
            let mut state = snapshot.write();
            if matches!(state.phase, PlaybackPhase::Playing | PlaybackPhase::Paused) {
                state.position = output.player.get_pos();
                if output.player.empty() {
                    let now = Instant::now();
                    if now >= suppress_empty_until {
                        let since = empty_since.get_or_insert(now);
                        match classify_empty_source(
                            now.duration_since(*since),
                            state.position,
                            state.duration,
                        ) {
                            EmptySource::Wait => {}
                            EmptySource::Ended => {
                                state.phase = PlaybackPhase::Ended;
                                if state.duration.is_zero() {
                                    state.duration = state.position;
                                }
                            }
                            EmptySource::Interrupted => {
                                state.phase = PlaybackPhase::Error;
                                state.error =
                                    Some("오디오 스트림이 곡이 끝나기 전에 중단되었습니다.".into());
                            }
                        }
                    }
                } else {
                    empty_since = None;
                }
            }
        }
    }
}

fn open_audio_output(volume: f32) -> Result<AudioOutput> {
    let host = cpal::default_host();
    let mut failures = Vec::new();

    if let Some(device) = host.default_output_device() {
        match open_audio_device(device, volume) {
            Ok(output) => return Ok(output),
            Err(error) => failures.push(format!("기본 장치: {error:#}")),
        }
    }

    for device in host
        .output_devices()
        .context("오디오 출력 장치 목록을 읽지 못했습니다")?
    {
        match open_audio_device(device, volume) {
            Ok(output) => return Ok(output),
            Err(error) => failures.push(format!("대체 장치: {error:#}")),
        }
    }

    if failures.is_empty() {
        anyhow::bail!("사용 가능한 출력 장치가 없습니다");
    }
    anyhow::bail!(failures.join("; "))
}

fn open_audio_device(device: cpal::Device, volume: f32) -> Result<AudioOutput> {
    let name = device
        .description()
        .map(|description| description.name().to_owned())
        .unwrap_or_else(|_| "이름 없는 장치".into());
    // Rodio's convenience opener first requests a fixed buffer size. Some
    // CoreAudio devices reject that even though their default stream config is
    // valid, so begin with the OS-selected buffer and then try every supported
    // configuration for this exact device.
    let sink = DeviceSinkBuilder::from_device(device)
        .and_then(|builder| {
            builder
                .with_buffer_size(cpal::BufferSize::Default)
                .open_sink_or_fallback()
        })
        .with_context(|| format!("'{name}'을(를) 열지 못했습니다"))?;
    let player = Player::connect_new(sink.mixer());
    player.set_volume(volume);
    Ok(AudioOutput {
        _sink: sink,
        player,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmptySource {
    Wait,
    Ended,
    Interrupted,
}

fn classify_empty_source(elapsed: Duration, position: Duration, duration: Duration) -> EmptySource {
    if elapsed < Duration::from_millis(300) {
        return EmptySource::Wait;
    }
    let near_known_end =
        !duration.is_zero() && position.saturating_add(Duration::from_secs(2)) >= duration;
    if near_known_end {
        EmptySource::Ended
    } else if elapsed >= Duration::from_secs(2) {
        EmptySource::Interrupted
    } else {
        EmptySource::Wait
    }
}

fn source_loader_loop(
    config: AppConfig,
    session_cache: Arc<tempfile::TempDir>,
    rx: mpsc::Receiver<LoadRequest>,
    tx: mpsc::Sender<PreparedSource>,
) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            log::error!("audio loader runtime unavailable: {error}");
            return;
        }
    };

    while let Ok(mut request) = rx.recv() {
        // Coalesce rapid clicks before opening an extractor. A request arriving while
        // yt-dlp is starting is still harmless: its stale result is discarded by the
        // generation check and this loop immediately advances to the newest request.
        while let Ok(newer) = rx.try_recv() {
            request = newer;
        }
        let result = open_source(&runtime, &config, session_cache.path(), &request.item)
            .map_err(|error| format!("{error:#}"));
        if tx
            .send(PreparedSource {
                generation: request.generation,
                result,
            })
            .is_err()
        {
            break;
        }
    }
}

fn apply_prepared_sources(
    rx: &mpsc::Receiver<PreparedSource>,
    player: &Player,
    snapshot: &RwLock<AudioSnapshot>,
) -> bool {
    let mut started = false;
    for prepared in rx.try_iter() {
        let current_generation = snapshot.read().generation;
        if prepared.generation != current_generation {
            continue;
        }
        match prepared.result {
            Ok((source, decoded_duration)) => {
                player.append(source);
                player.set_volume(snapshot.read().volume);
                player.play();
                let mut state = snapshot.write();
                if state.generation != prepared.generation {
                    player.stop();
                    continue;
                }
                if state.duration.is_zero()
                    && let Some(duration) = decoded_duration
                {
                    state.duration = duration;
                }
                state.phase = PlaybackPhase::Playing;
                started = true;
            }
            Err(error) => set_error(snapshot, format!("재생 스트림을 열 수 없습니다: {error}")),
        }
    }
    started
}

fn prefetch_loop(
    config: AppConfig,
    session_cache: Arc<tempfile::TempDir>,
    rx: mpsc::Receiver<Vec<MediaItem>>,
) {
    let mut targets = match rx.recv() {
        Ok(targets) => targets,
        Err(_) => return,
    };

    'requests: loop {
        while let Ok(newer) = rx.try_recv() {
            targets = newer;
        }
        // Starting a second yt-dlp + FFmpeg pair while the current track is
        // still filling its .part file roughly doubles the process working set.
        // Wait for that initial stream to finish, while still reacting instantly
        // if the queue changes.
        let wait_started = Instant::now();
        loop {
            let active_download = std::fs::read_dir(session_cache.path())
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .any(|entry| {
                    entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "part")
                });
            if wait_started.elapsed() >= PREFETCH_START_DELAY && !active_download {
                break;
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(newer) => {
                    targets = newer;
                    continue 'requests;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        for item in &targets {
            let Some(url) = item.watch_url() else {
                continue;
            };
            let paths = cache_paths(session_cache.path(), item);
            if paths.audio.is_file() {
                continue;
            }
            let mut command = match media_helper_command(
                &config,
                &url,
                "bestaudio/best",
                &paths.audio,
                &paths.duration,
            ) {
                Ok(command) => command,
                Err(error) => {
                    log::warn!("prefetch helper unavailable: {error:#}");
                    continue;
                }
            };
            let mut child = match command.stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
                Ok(child) => child,
                Err(error) => {
                    log::warn!("prefetch process failed to start: {error}");
                    continue;
                }
            };

            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        if !status.success() {
                            log::debug!("track prefetch did not complete for {}", item.id);
                        }
                        break;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        log::warn!("track prefetch process failed: {error}");
                        break;
                    }
                }
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(newer) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        targets = newer;
                        continue 'requests;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        }

        targets = match rx.recv() {
            Ok(targets) => targets,
            Err(_) => return,
        };
    }
}

#[derive(Debug)]
struct CachePaths {
    audio: PathBuf,
    duration: PathBuf,
}

fn cache_paths(root: &Path, item: &MediaItem) -> CachePaths {
    let identity = item.video_id.as_deref().unwrap_or(&item.id);
    let digest = Sha256::digest(identity.as_bytes());
    let key = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    CachePaths {
        audio: root.join(format!("{key}.wav")),
        duration: root.join(format!("{key}.duration")),
    }
}

fn open_source(
    runtime: &tokio::runtime::Runtime,
    config: &AppConfig,
    cache_root: &Path,
    item: &MediaItem,
) -> Result<(NativeSource, Option<Duration>)> {
    let url = item
        .watch_url()
        .context("선택한 항목에 YouTube videoId가 없습니다")?;
    open_source_with_format(runtime, config, cache_root, item, &url, "bestaudio/best")
}

fn open_source_with_format(
    runtime: &tokio::runtime::Runtime,
    config: &AppConfig,
    cache_root: &Path,
    item: &MediaItem,
    url: &str,
    format: &str,
) -> Result<(NativeSource, Option<Duration>)> {
    let paths = cache_paths(cache_root, item);
    if paths.audio.is_file() {
        let file = File::open(&paths.audio).context("세션 오디오 캐시를 열지 못했습니다")?;
        let decoder =
            Decoder::new(BufReader::new(file)).context("세션 오디오 캐시가 손상되었습니다")?;
        return Ok((Box::new(decoder), read_duration(&paths.duration)));
    }

    let stderr_file =
        tempfile::NamedTempFile::new().context("오디오 오류 로그를 만들지 못했습니다")?;
    let stderr_handle = stderr_file
        .as_file()
        .try_clone()
        .context("오디오 오류 로그를 복제하지 못했습니다")?;
    let helper = media_helper_command(config, url, format, &paths.audio, &paths.duration)?;
    let helper = StreamCommand::new(helper.get_program())
        .args(helper.get_args())
        .stderr_handle(Stdio::from(stderr_handle));

    let reader = runtime.block_on(async move {
        let params = ProcessStreamParams::new(helper)
            .context("오디오 스트리밍 프로세스를 시작하지 못했습니다")?;
        let reader = StreamDownload::new_process(
            params,
            TempStorageProvider::new(),
            Settings::default()
                .prefetch_bytes(STREAM_PREFETCH_BYTES)
                .retry_timeout(Duration::from_secs(30)),
        )
        .await?;
        Ok::<_, anyhow::Error>(reader)
    })?;
    let decoder = match Decoder::new(Box::new(reader)) {
        Ok(decoder) => decoder,
        Err(decode_error) => {
            let mut stderr = String::new();
            let _ = stderr_file
                .reopen()
                .and_then(|mut file| file.read_to_string(&mut stderr));
            let stderr = stderr.trim();
            if stderr.is_empty() {
                return Err(decode_error)
                    .context("ffmpeg가 재생 가능한 오디오 스트림을 만들지 못했습니다");
            }
            let concise = stderr.lines().last().unwrap_or(stderr);
            anyhow::bail!("yt-dlp가 오디오를 내려받지 못했습니다: {concise}");
        }
    };
    Ok((Box::new(decoder), read_duration(&paths.duration)))
}

fn read_duration(path: &Path) -> Option<Duration> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|value| value.lines().last()?.trim().parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .map(Duration::from_secs_f64)
}

fn media_helper_command(
    config: &AppConfig,
    url: &str,
    format: &str,
    cache_path: &Path,
    duration_path: &Path,
) -> Result<StdCommand> {
    let executable = std::env::current_exe().context("현재 앱 실행 파일을 찾지 못했습니다")?;
    let mut command = StdCommand::new(executable);
    command
        .arg(MEDIA_STREAM_FLAG)
        .arg("--yt-dlp")
        .arg(&config.yt_dlp)
        .arg("--ffmpeg")
        .arg(&config.ffmpeg)
        .arg("--url")
        .arg(url)
        .arg("--format")
        .arg(format)
        .arg("--cache")
        .arg(cache_path)
        .arg("--duration")
        .arg(duration_path);
    if let Some(deno) = &config.deno {
        command.arg("--deno").arg(deno);
    }
    if let Some(cookies) = &config.cookies_path
        && cookies.exists()
    {
        command.arg("--cookies").arg(cookies);
    }
    Ok(command)
}

/// Runs the executable's hidden media mode before GPUI is initialized. The helper
/// tees normalized WAV chunks to stdout and a session-only cache file.
pub fn maybe_run_media_stream() -> Option<i32> {
    if std::env::args_os().nth(1).as_deref() != Some(OsStr::new(MEDIA_STREAM_FLAG)) {
        return None;
    }
    match media_stream_main(std::env::args_os().skip(2)) {
        Ok(()) => Some(0),
        Err(error) => {
            eprintln!("{error:#}");
            Some(1)
        }
    }
}

fn media_stream_main(args: impl Iterator<Item = OsString>) -> Result<()> {
    let args = parse_media_args(args)?;
    if let Some(parent) = args.cache.parent() {
        std::fs::create_dir_all(parent).context("세션 캐시 폴더를 만들지 못했습니다")?;
    }
    let process_id = std::process::id();
    let cache_part = sibling_temp_path(&args.cache, process_id, "part");
    let duration_part = sibling_temp_path(&args.duration, process_id, "metadata");

    let mut yt_dlp = StdCommand::new(&args.yt_dlp);
    yt_dlp
        .arg(&args.url)
        .args([
            "--quiet",
            "--ignore-config",
            "--no-update",
            "--no-part",
            "--no-continue",
            "--no-playlist",
        ])
        .args(["-f", &args.format, "-o", "-"])
        .arg("--ffmpeg-location")
        .arg(&args.ffmpeg)
        .arg("--print-to-file")
        .arg("before_dl:%(duration)s")
        .arg(&duration_part)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(cookies) = &args.cookies {
        yt_dlp.arg("--cookies").arg(cookies);
    }
    if let Some(deno) = &args.deno {
        yt_dlp
            .arg("--js-runtimes")
            .arg(format!("deno:{}", deno.display()));
    }
    let mut yt_dlp = yt_dlp
        .spawn()
        .with_context(|| format!("'{}' 실행 파일을 시작하지 못했습니다", args.yt_dlp))?;
    let yt_stdout = yt_dlp
        .stdout
        .take()
        .context("yt-dlp 오디오 출력을 열지 못했습니다")?;

    let mut ffmpeg = StdCommand::new(&args.ffmpeg)
        .args([
            "-i",
            "pipe:",
            "-map",
            "a",
            "-f",
            "wav",
            "-loglevel",
            "error",
            "-vn",
            "-acodec",
            "pcm_s16le",
            "-",
        ])
        .stdin(Stdio::from(yt_stdout))
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("'{}' 실행 파일을 시작하지 못했습니다", args.ffmpeg))?;
    let mut ffmpeg_stdout = ffmpeg
        .stdout
        .take()
        .context("ffmpeg 오디오 출력을 열지 못했습니다")?;
    let mut cache = File::create(&cache_part).context("세션 캐시 파일을 만들지 못했습니다")?;
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut written = 0_u64;
    let copy_result = (|| -> Result<()> {
        loop {
            let count = ffmpeg_stdout
                .read(&mut buffer)
                .context("ffmpeg 오디오를 읽지 못했습니다")?;
            if count == 0 {
                break;
            }
            output
                .write_all(&buffer[..count])
                .context("재생 스트림을 전달하지 못했습니다")?;
            cache
                .write_all(&buffer[..count])
                .context("세션 오디오 캐시에 쓰지 못했습니다")?;
            written += count as u64;
            publish_duration(&duration_part, &args.duration);
        }
        output
            .flush()
            .context("재생 스트림을 마무리하지 못했습니다")?;
        cache
            .sync_all()
            .context("세션 오디오 캐시를 저장하지 못했습니다")?;
        Ok(())
    })();

    if copy_result.is_err() {
        let _ = ffmpeg.kill();
        let _ = yt_dlp.kill();
    }
    let ffmpeg_status = ffmpeg
        .wait()
        .context("ffmpeg 종료 상태를 읽지 못했습니다")?;
    let yt_dlp_status = yt_dlp
        .wait()
        .context("yt-dlp 종료 상태를 읽지 못했습니다")?;
    publish_duration(&duration_part, &args.duration);

    if let Err(error) = copy_result {
        let _ = std::fs::remove_file(&cache_part);
        let _ = std::fs::remove_file(&duration_part);
        return Err(error);
    }
    if !yt_dlp_status.success() || !ffmpeg_status.success() || written <= 44 {
        let _ = std::fs::remove_file(&cache_part);
        let _ = std::fs::remove_file(&duration_part);
        anyhow::bail!(
            "오디오 파이프라인이 완료되지 않았습니다 (yt-dlp: {yt_dlp_status}, ffmpeg: {ffmpeg_status})"
        );
    }

    if args.cache.is_file() {
        std::fs::remove_file(&cache_part).ok();
    } else if let Err(error) = std::fs::rename(&cache_part, &args.cache) {
        if args.cache.is_file() {
            std::fs::remove_file(&cache_part).ok();
        } else {
            return Err(error).context("세션 오디오 캐시를 확정하지 못했습니다");
        }
    }
    std::fs::remove_file(&duration_part).ok();
    Ok(())
}

#[derive(Debug)]
struct MediaArgs {
    yt_dlp: String,
    ffmpeg: String,
    deno: Option<PathBuf>,
    url: String,
    format: String,
    cache: PathBuf,
    duration: PathBuf,
    cookies: Option<PathBuf>,
}

fn parse_media_args(mut args: impl Iterator<Item = OsString>) -> Result<MediaArgs> {
    let mut values = std::collections::HashMap::new();
    while let Some(flag) = args.next() {
        let flag = flag.to_string_lossy().into_owned();
        let value = args
            .next()
            .with_context(|| format!("{flag} 값이 없습니다"))?;
        values.insert(flag, value);
    }
    let required = |flag: &str| -> Result<OsString> {
        values
            .get(flag)
            .cloned()
            .with_context(|| format!("{flag} 옵션이 없습니다"))
    };
    Ok(MediaArgs {
        yt_dlp: required("--yt-dlp")?.to_string_lossy().into_owned(),
        ffmpeg: required("--ffmpeg")?.to_string_lossy().into_owned(),
        deno: values.get("--deno").map(PathBuf::from),
        url: required("--url")?.to_string_lossy().into_owned(),
        format: required("--format")?.to_string_lossy().into_owned(),
        cache: PathBuf::from(required("--cache")?),
        duration: PathBuf::from(required("--duration")?),
        cookies: values.get("--cookies").map(PathBuf::from),
    })
}

fn sibling_temp_path(path: &Path, process_id: u32, suffix: &str) -> PathBuf {
    let name = path.file_name().and_then(OsStr::to_str).unwrap_or("audio");
    path.with_file_name(format!("{name}.{process_id}.{suffix}"))
}

fn publish_duration(source: &Path, destination: &Path) {
    let Some(duration) = read_duration(source) else {
        return;
    };
    let temporary = destination.with_extension(format!("duration.{}.tmp", std::process::id()));
    if std::fs::write(&temporary, format!("{}\n", duration.as_secs_f64())).is_err() {
        return;
    }
    if destination.is_file() || std::fs::rename(&temporary, destination).is_err() {
        std::fs::remove_file(&temporary).ok();
    }
}

fn set_error(snapshot: &RwLock<AudioSnapshot>, error: String) {
    let mut state = snapshot.write();
    state.phase = PlaybackPhase::Error;
    state.error = Some(error);
}

#[derive(Debug, Deserialize, Serialize)]
struct PersistentAudioSettings {
    volume: f32,
}

fn load_saved_volume(path: &Path) -> Option<f32> {
    let settings: PersistentAudioSettings =
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    settings
        .volume
        .is_finite()
        .then(|| settings.volume.clamp(0.0, 1.0))
}

fn save_volume(path: &Path, volume: f32) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("설정 폴더를 만들지 못했습니다")?;
    }
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(&PersistentAudioSettings {
        volume: volume.clamp(0.0, 1.0),
    })?;
    std::fs::write(&temporary, bytes).context("임시 설정 파일을 쓰지 못했습니다")?;
    std::fs::rename(&temporary, path).context("볼륨 설정을 확정하지 못했습니다")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(video_id: &str) -> MediaItem {
        MediaItem {
            id: video_id.into(),
            video_id: Some(video_id.into()),
            ..Default::default()
        }
    }

    #[test]
    fn cache_is_stable_per_track_and_scoped_to_the_given_session() {
        let first_session = tempfile::tempdir().unwrap();
        let second_session = tempfile::tempdir().unwrap();
        let first = cache_paths(first_session.path(), &item("abc"));
        let again = cache_paths(first_session.path(), &item("abc"));
        let other_track = cache_paths(first_session.path(), &item("xyz"));
        let other_session = cache_paths(second_session.path(), &item("abc"));

        assert_eq!(first.audio, again.audio);
        assert_ne!(first.audio, other_track.audio);
        assert_ne!(first.audio, other_session.audio);
        assert!(first.audio.starts_with(first_session.path()));
    }

    #[test]
    fn hidden_media_arguments_round_trip_paths() {
        let args = [
            "--yt-dlp",
            "yt-dlp",
            "--ffmpeg",
            "ffmpeg",
            "--deno",
            "/Applications/Pocket Music.app/Contents/Resources/bin/deno",
            "--url",
            "https://music.youtube.com/watch?v=abc",
            "--format",
            "bestaudio/best",
            "--cache",
            "/tmp/cache.wav",
            "--duration",
            "/tmp/cache.duration",
        ]
        .into_iter()
        .map(OsString::from);
        let parsed = parse_media_args(args).unwrap();
        assert_eq!(parsed.cache, Path::new("/tmp/cache.wav"));
        assert_eq!(parsed.duration, Path::new("/tmp/cache.duration"));
        assert_eq!(
            parsed.deno.as_deref(),
            Some(Path::new(
                "/Applications/Pocket Music.app/Contents/Resources/bin/deno"
            ))
        );
        assert!(parsed.cookies.is_none());
    }

    #[test]
    fn volume_round_trips_through_persistent_settings() {
        let root = tempfile::tempdir().unwrap();
        let settings = root.path().join("nested/settings.json");

        save_volume(&settings, 0.35).unwrap();

        assert_eq!(load_saved_volume(&settings), Some(0.35));
    }

    #[test]
    fn persisted_volume_is_clamped_and_invalid_json_is_ignored() {
        let root = tempfile::tempdir().unwrap();
        let settings = root.path().join("settings.json");
        save_volume(&settings, 2.0).unwrap();
        assert_eq!(load_saved_volume(&settings), Some(1.0));

        std::fs::write(&settings, b"not json").unwrap();
        assert_eq!(load_saved_volume(&settings), None);
    }

    #[test]
    fn a_transient_empty_source_during_seek_is_not_the_end_of_the_track() {
        assert_eq!(
            classify_empty_source(
                Duration::from_millis(500),
                Duration::from_secs(100),
                Duration::from_secs(200),
            ),
            EmptySource::Wait
        );
        assert_eq!(
            classify_empty_source(
                Duration::from_millis(500),
                Duration::from_secs(199),
                Duration::from_secs(200),
            ),
            EmptySource::Ended
        );
        assert_eq!(
            classify_empty_source(
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::ZERO,
            ),
            EmptySource::Wait
        );
    }

    #[test]
    fn startup_removes_only_stale_app_session_cache_directories() {
        let root = tempfile::tempdir().unwrap();
        let stale = root.path().join(format!("{SESSION_CACHE_PREFIX}stale"));
        let unrelated = root.path().join("some-other-temp-dir");
        let similarly_named_file = root.path().join(format!("{SESSION_CACHE_PREFIX}file"));
        std::fs::create_dir(&stale).unwrap();
        std::fs::create_dir(&unrelated).unwrap();
        std::fs::write(&similarly_named_file, b"keep").unwrap();

        purge_session_cache_dirs(root.path());

        assert!(!stale.exists());
        assert!(unrelated.exists());
        assert!(similarly_named_file.exists());
    }
}
