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
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use parking_lot::RwLock;
use reqwest::{
    StatusCode,
    blocking::Client as BlockingClient,
    header::{ACCEPT_ENCODING, CONTENT_RANGE, HeaderMap, HeaderName, HeaderValue, RANGE},
};
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

use crate::{
    config::AppConfig,
    model::MediaItem,
    resolver::{MediaResolver, RESOLVER_PROFILE_COUNT, ResolvedMedia},
};

const MEDIA_STREAM_FLAG: &str = "--pocket-music-media-stream";
const SESSION_CACHE_PREFIX: &str = "pocket-music-session-";
const STREAM_PREFETCH_BYTES: u64 = 1024 * 128;
const HTTP_RANGE_CHUNK_BYTES: u64 = 512 * 1024;
const HTTP_RANGE_MAX_RETRIES: usize = 3;
const PREFETCH_START_DELAY: Duration = Duration::from_secs(1);
const OUTPUT_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const BUFFER_REFRESH_INTERVAL: Duration = Duration::from_millis(100);
const NORMALIZED_SAMPLE_RATE: u64 = 48_000;
const NORMALIZED_CHANNELS: u64 = 2;
const NORMALIZED_BYTES_PER_SAMPLE: u64 = 2;
const WAV_HEADER_ESTIMATE: u64 = 78;

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
    pub replacement: Option<PlaybackReplacement>,
    pub buffered_ranges: Vec<BufferedRange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferedRange {
    pub start: Duration,
    pub end: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackReplacement {
    pub title: String,
    pub video_id: String,
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
            replacement: None,
            buffered_ranges: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct AudioEngine {
    tx: mpsc::Sender<AudioCommand>,
    snapshot: Arc<RwLock<AudioSnapshot>>,
    resolver: Arc<MediaResolver>,
    // The cache directory lives exactly as long as the running app session. TempDir
    // removes every downloaded track after the final engine/worker is dropped.
    _session_cache: Arc<tempfile::TempDir>,
}

enum AudioCommand {
    Load(Box<LoadRequest>),
    Prefetch(Vec<MediaItem>),
    Toggle,
    Seek(Duration),
    SetVolume(f32),
}

struct LoadRequest {
    generation: u64,
    item: MediaItem,
    start_position: Duration,
    start_paused: bool,
}

struct PreparedSource {
    generation: u64,
    start_position: Duration,
    start_paused: bool,
    result: Result<OpenedSource, String>,
}

struct AudioOutput {
    _sink: MixerDeviceSink,
    player: Player,
}

type NativeSource = Box<dyn Source + Send>;
type OpenedSource = (NativeSource, Option<Duration>, Option<PlaybackReplacement>);

impl AudioEngine {
    pub fn new(config: AppConfig) -> Self {
        purge_session_cache_dirs(&std::env::temp_dir());
        let resolver = MediaResolver::new(&config);
        resolver.warm_up();
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
        let thread_resolver = resolver.clone();
        thread::Builder::new()
            .name("pocket-ytm-audio".into())
            .spawn(move || audio_loop(config, thread_resolver, thread_cache, rx, thread_snapshot))
            .expect("failed to start native audio thread");
        Self {
            tx,
            snapshot,
            resolver,
            _session_cache: session_cache,
        }
    }

    pub fn public_playlist_queue(&self, playlist_id: &str) -> Result<crate::model::WatchQueue> {
        self.resolver.playlist_queue(playlist_id, 50)
    }

    pub fn snapshot(&self) -> AudioSnapshot {
        self.snapshot.read().clone()
    }

    pub fn load(&self, item: MediaItem) {
        let generation = {
            let mut state = self.snapshot.write();
            state.generation = state.generation.wrapping_add(1);
            state.phase = PlaybackPhase::Loading;
            state.position = Duration::ZERO;
            state.duration = item
                .duration_seconds
                .map(Duration::from_secs)
                .unwrap_or_default();
            state.item = Some(item.clone());
            state.error = None;
            state.replacement = None;
            state.buffered_ranges.clear();
            state.generation
        };
        let request = LoadRequest {
            generation,
            item,
            start_position: Duration::ZERO,
            start_paused: false,
        };
        if self.tx.send(AudioCommand::Load(Box::new(request))).is_err() {
            set_error(
                &self.snapshot,
                "재생 엔진이 종료되어 재생 명령을 전달하지 못했습니다. 앱을 다시 실행해 주세요."
                    .into(),
            );
        }
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
    resolver: Arc<MediaResolver>,
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
    let loader_resolver = resolver.clone();
    let loader_cache = session_cache.clone();
    thread::Builder::new()
        .name("pocket-ytm-source-loader".into())
        .spawn(move || {
            source_loader_loop(
                loader_config,
                loader_resolver,
                loader_cache,
                load_rx,
                prepared_tx,
            )
        })
        .expect("failed to start source loader");

    let (prefetch_tx, prefetch_rx) = mpsc::channel();
    let prefetch_config = config.clone();
    let prefetch_resolver = resolver;
    let prefetch_cache = session_cache.clone();
    thread::Builder::new()
        .name("pocket-ytm-prefetch".into())
        .spawn(move || {
            prefetch_loop(
                prefetch_config,
                prefetch_resolver,
                prefetch_cache,
                prefetch_rx,
            )
        })
        .expect("failed to start source prefetcher");

    let mut empty_since = None;
    let mut suppress_empty_until = Instant::now();
    let mut pending_load: Option<LoadRequest> = None;
    let mut next_output_retry = Instant::now();
    let mut attached_generation = None;
    let mut attached_at = None;
    let mut position_base = Duration::ZERO;
    let mut active_buffered_path: Option<PathBuf> = None;
    let mut next_buffer_refresh = Instant::now();

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
            && let Some((generation, start_position)) =
                apply_prepared_sources(&prepared_rx, &output.player, &snapshot)
        {
            attached_generation = Some(generation);
            attached_at = Some(Instant::now());
            position_base = start_position;
            empty_since = None;
            suppress_empty_until = Instant::now() + Duration::from_secs(1);
        }

        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(AudioCommand::Load(request)) => {
                let request = *request;
                active_buffered_path = Some(
                    cache_paths(session_cache.path(), &request.item)
                        .buffered
                        .clone(),
                );
                if let Some(output) = &output {
                    output.player.stop();
                }
                empty_since = None;
                attached_generation = None;
                attached_at = None;
                position_base = Duration::ZERO;
                suppress_empty_until = Instant::now() + Duration::from_secs(1);
                next_buffer_refresh = Instant::now();
                if output.is_some() {
                    if load_tx.send(request).is_err() {
                        set_error(
                            &snapshot,
                            "오디오 소스 로더가 종료되어 스트림을 준비하지 못했습니다.".into(),
                        );
                    } else {
                        pending_load = None;
                    }
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
                let seek_within_active_buffer = active_buffered_path
                    .as_deref()
                    .and_then(read_buffered_progress)
                    .is_some_and(|progress| {
                        let target = position.as_secs_f64();
                        target >= position_base.as_secs_f64()
                            && target >= progress.start_seconds
                            && target <= progress.end_seconds
                    });
                if seek_within_active_buffer
                    && let Some(output) = &output
                    && output
                        .player
                        .try_seek(position.saturating_sub(position_base))
                        .is_ok()
                {
                    let mut state = snapshot.write();
                    state.position = position;
                    state.error = None;
                    empty_since = None;
                    suppress_empty_until = Instant::now() + Duration::from_secs(1);
                    continue;
                }
                let request = {
                    let mut state = snapshot.write();
                    let Some(item) = state.item.clone() else {
                        continue;
                    };
                    let start_paused = state.phase == PlaybackPhase::Paused;
                    state.generation = state.generation.wrapping_add(1);
                    state.phase = PlaybackPhase::Loading;
                    state.position = position;
                    state.error = None;
                    LoadRequest {
                        generation: state.generation,
                        item,
                        start_position: position,
                        start_paused,
                    }
                };
                active_buffered_path = Some(
                    seek_cache_paths(session_cache.path(), &request.item, request.generation)
                        .buffered,
                );
                if let Some(output) = &output {
                    output.player.stop();
                }
                empty_since = None;
                attached_generation = None;
                attached_at = None;
                suppress_empty_until = Instant::now() + Duration::from_secs(1);
                next_buffer_refresh = Instant::now();
                if output.is_some() {
                    if load_tx.send(request).is_err() {
                        set_error(
                            &snapshot,
                            "오디오 소스 로더가 종료되어 탐색하지 못했습니다.".into(),
                        );
                    } else {
                        pending_load = None;
                    }
                } else {
                    pending_load = Some(request);
                    next_output_retry = Instant::now();
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

        if Instant::now() >= next_buffer_refresh {
            refresh_buffered_ranges(session_cache.path(), &snapshot);
            next_buffer_refresh = Instant::now() + BUFFER_REFRESH_INTERVAL;
        }

        if let Some(output) = &output {
            if let Some((generation, start_position)) =
                apply_prepared_sources(&prepared_rx, &output.player, &snapshot)
            {
                attached_generation = Some(generation);
                attached_at = Some(Instant::now());
                position_base = start_position;
                empty_since = None;
                suppress_empty_until = Instant::now() + Duration::from_secs(1);
            }
            let mut state = snapshot.write();
            let attached_loading = state.phase == PlaybackPhase::Loading
                && attached_generation == Some(state.generation);
            if attached_loading
                || matches!(state.phase, PlaybackPhase::Playing | PlaybackPhase::Paused)
            {
                let relative_position = output.player.get_pos();
                state.position = position_base.saturating_add(relative_position);
                if attached_loading && !relative_position.is_zero() && !output.player.empty() {
                    state.phase = PlaybackPhase::Playing;
                    attached_at = None;
                }
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
                    if state.phase == PlaybackPhase::Loading
                        && attached_at
                            .is_some_and(|started| started.elapsed() >= Duration::from_secs(15))
                    {
                        state.phase = PlaybackPhase::Error;
                        state.error = Some(
                            "오디오 디코더가 15초 동안 재생 샘플을 공급하지 못했습니다.".into(),
                        );
                    }
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
    let end_tolerance = Duration::from_secs_f64((duration.as_secs_f64() * 0.02).clamp(5.0, 15.0));
    let near_known_end = !duration.is_zero() && position.saturating_add(end_tolerance) >= duration;
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
    resolver: Arc<MediaResolver>,
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
        let result = open_source(
            &runtime,
            &config,
            &resolver,
            session_cache.path(),
            &request.item,
            request.start_position,
            request.generation,
        )
        .map_err(|error| format!("{error:#}"));
        if tx
            .send(PreparedSource {
                generation: request.generation,
                start_position: request.start_position,
                start_paused: request.start_paused,
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
) -> Option<(u64, Duration)> {
    let mut attached = None;
    for prepared in rx.try_iter() {
        let current_generation = snapshot.read().generation;
        if prepared.generation != current_generation {
            continue;
        }
        match prepared.result {
            Ok((source, decoded_duration, replacement)) => {
                player.append(source);
                player.set_volume(snapshot.read().volume);
                let mut state = snapshot.write();
                if state.generation != prepared.generation {
                    player.stop();
                    continue;
                }
                // The catalog duration can differ from the actual fallback/client
                // stream (for example an Android combined format or a replacement
                // upload). The seek bar must describe the source being played.
                if let Some(duration) = decoded_duration {
                    state.duration = duration;
                }
                state.position = prepared.start_position;
                state.replacement = replacement;
                // Appending a decoder only means that the source is attached. Keep
                // showing Loading until the audio device reports a non-zero playback
                // position, which proves that actual samples reached the mixer.
                if prepared.start_paused {
                    player.pause();
                    state.phase = PlaybackPhase::Paused;
                } else {
                    player.play();
                    state.phase = PlaybackPhase::Loading;
                }
                attached = Some((prepared.generation, prepared.start_position));
            }
            Err(error) => set_error(snapshot, format!("재생 스트림을 열 수 없습니다: {error}")),
        }
    }
    attached
}

fn prefetch_loop(
    config: AppConfig,
    resolver: Arc<MediaResolver>,
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
            let paths = cache_paths(session_cache.path(), item);
            if paths.audio.is_file() {
                continue;
            }
            let resolved = match resolve_prefetch_item(&resolver, item, "bestaudio/best") {
                Ok(resolved) => resolved,
                Err(error) => {
                    log::warn!("track prefetch resolution failed: {error:#}");
                    continue;
                }
            };
            let mut command = match media_helper_command(
                &config,
                &resolved,
                &paths.audio,
                &paths.duration,
                &paths.buffered,
                Duration::ZERO,
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

#[derive(Debug, Clone)]
struct CachePaths {
    audio: PathBuf,
    duration: PathBuf,
    replacement: PathBuf,
    buffered: PathBuf,
}

fn cache_key(item: &MediaItem) -> String {
    let identity = item.video_id.as_deref().unwrap_or(&item.id);
    let digest = Sha256::digest(identity.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn cache_paths_for_stem(root: &Path, stem: &str) -> CachePaths {
    CachePaths {
        audio: root.join(format!("{stem}.wav")),
        duration: root.join(format!("{stem}.duration")),
        replacement: root.join(format!("{stem}.replacement.json")),
        buffered: root.join(format!("{stem}.buffered.json")),
    }
}

fn cache_paths(root: &Path, item: &MediaItem) -> CachePaths {
    cache_paths_for_stem(root, &cache_key(item))
}

fn seek_cache_paths(root: &Path, item: &MediaItem, generation: u64) -> CachePaths {
    cache_paths_for_stem(root, &format!("{}.seek-{generation}", cache_key(item)))
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BufferedProgress {
    start_seconds: f64,
    end_seconds: f64,
    complete: bool,
}

#[derive(Debug)]
struct BufferedCacheEntry {
    paths: CachePaths,
    progress: BufferedProgress,
}

fn read_buffered_progress(path: &Path) -> Option<BufferedProgress> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn read_buffered_cache_entries(root: &Path, item: &MediaItem) -> Vec<BufferedCacheEntry> {
    let prefix = format!("{}.", cache_key(item));
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let file_name = entry.file_name();
            let file_name = file_name.to_str()?;
            if !file_name.starts_with(&prefix) || !file_name.ends_with(".buffered.json") {
                return None;
            }
            let stem = file_name.strip_suffix(".buffered.json")?;
            let progress = read_buffered_progress(&entry.path())?;
            if !progress.start_seconds.is_finite()
                || !progress.end_seconds.is_finite()
                || progress.start_seconds < 0.0
                || progress.end_seconds < progress.start_seconds
            {
                return None;
            }
            Some(BufferedCacheEntry {
                paths: cache_paths_for_stem(root, stem),
                progress,
            })
        })
        .collect()
}

fn merged_buffered_ranges(root: &Path, item: &MediaItem) -> Vec<BufferedRange> {
    let mut ranges = read_buffered_cache_entries(root, item)
        .into_iter()
        .filter_map(|entry| {
            (entry.progress.end_seconds > entry.progress.start_seconds).then_some(BufferedRange {
                start: Duration::from_secs_f64(entry.progress.start_seconds),
                end: Duration::from_secs_f64(entry.progress.end_seconds),
            })
        })
        .collect::<Vec<_>>();
    ranges.sort_unstable_by_key(|range| range.start);
    let mut merged: Vec<BufferedRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if let Some(previous) = merged.last_mut()
            && range.start <= previous.end.saturating_add(Duration::from_millis(250))
        {
            previous.end = previous.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    merged
}

fn refresh_buffered_ranges(root: &Path, snapshot: &RwLock<AudioSnapshot>) {
    let Some(item) = snapshot.read().item.clone() else {
        return;
    };
    let ranges = merged_buffered_ranges(root, &item);
    let mut state = snapshot.write();
    if state
        .item
        .as_ref()
        .is_some_and(|current| current.id == item.id)
    {
        state.buffered_ranges = ranges;
    }
}

fn completed_cached_segment(
    root: &Path,
    item: &MediaItem,
    position: Duration,
) -> Option<(CachePaths, Duration)> {
    let seconds = position.as_secs_f64();
    read_buffered_cache_entries(root, item)
        .into_iter()
        .filter(|entry| {
            entry.progress.complete
                && entry.paths.audio.is_file()
                && entry.progress.start_seconds <= seconds
                && seconds <= entry.progress.end_seconds
        })
        .max_by(|left, right| {
            left.progress
                .start_seconds
                .total_cmp(&right.progress.start_seconds)
        })
        .map(|entry| {
            (
                entry.paths,
                Duration::from_secs_f64(entry.progress.start_seconds),
            )
        })
}

fn open_source(
    runtime: &tokio::runtime::Runtime,
    config: &AppConfig,
    resolver: &MediaResolver,
    cache_root: &Path,
    item: &MediaItem,
    start_position: Duration,
    generation: u64,
) -> Result<OpenedSource> {
    let format = "bestaudio/best";
    let canonical_paths = cache_paths(cache_root, item);
    if start_position.is_zero() && canonical_paths.audio.is_file() {
        let file =
            File::open(&canonical_paths.audio).context("세션 오디오 캐시를 열지 못했습니다")?;
        let decoder =
            Decoder::new(BufReader::new(file)).context("세션 오디오 캐시가 손상되었습니다")?;
        return Ok((
            Box::new(decoder),
            read_duration(&canonical_paths.duration),
            read_replacement(&canonical_paths.replacement),
        ));
    }
    if let Some((cached_paths, segment_start)) =
        completed_cached_segment(cache_root, item, start_position)
    {
        let file = File::open(&cached_paths.audio)
            .context("탐색할 세션 오디오 구간 캐시를 열지 못했습니다")?;
        let mut decoder =
            Decoder::new(BufReader::new(file)).context("세션 오디오 구간 캐시가 손상되었습니다")?;
        decoder
            .try_seek(start_position.saturating_sub(segment_start))
            .context("세션 오디오 구간 캐시에서 재생 위치를 찾지 못했습니다")?;
        return Ok((
            Box::new(decoder),
            read_duration(&cached_paths.duration),
            read_replacement(&cached_paths.replacement),
        ));
    }
    let paths = if start_position.is_zero() {
        canonical_paths
    } else {
        seek_cache_paths(cache_root, item, generation)
    };

    let mut failures = Vec::with_capacity(RESOLVER_PROFILE_COUNT * 3);
    if let Some(url) = item.watch_url() {
        for profile in 0..RESOLVER_PROFILE_COUNT {
            let resolved = match resolver.resolve_profile(&url, format, profile) {
                Ok(resolved) => resolved,
                Err(error) => {
                    failures.push(format!("프로필 {} 해석: {error:#}", profile + 1));
                    continue;
                }
            };
            match open_resolved_source(runtime, config, &paths, &resolved, start_position) {
                Ok(source) => return Ok(source),
                Err(error) => failures.push(format!("프로필 {} 스트림: {error:#}", profile + 1)),
            }
        }
    }

    match canonical_collection_candidate(resolver, item) {
        Ok(Some(candidate)) => {
            let url = candidate
                .watch_url()
                .context("원본 컬렉션 후보에 YouTube 영상 ID가 없습니다")?;
            for profile in 0..RESOLVER_PROFILE_COUNT {
                let mut resolved = match resolver.resolve_profile(&url, format, profile) {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        failures.push(format!(
                            "원본 컬렉션 프로필 {} 해석: {error:#}",
                            profile + 1
                        ));
                        continue;
                    }
                };
                resolved.video_id.clone_from(&candidate.video_id);
                resolved.title = Some(candidate.title.clone());
                match open_resolved_source(runtime, config, &paths, &resolved, start_position) {
                    Ok(source) => return Ok(source),
                    Err(error) => failures.push(format!(
                        "원본 컬렉션 프로필 {} 스트림: {error:#}",
                        profile + 1
                    )),
                }
            }
        }
        Ok(None) => {}
        Err(error) => failures.push(format!("원본 컬렉션 확인: {error:#}")),
    }

    let fallback_query = if item.has_direct_video() {
        item.direct_audio_fallback_query()
    } else {
        item.fallback_search_query()
    };
    if let Some(query) = fallback_query {
        for profile in 0..RESOLVER_PROFILE_COUNT {
            let resolved = match resolver.search_profile(&query, format, profile) {
                Ok(resolved) => resolved,
                Err(error) => {
                    failures.push(format!("검색 대체 프로필 {} 해석: {error:#}", profile + 1));
                    continue;
                }
            };
            match open_resolved_source(runtime, config, &paths, &resolved, start_position) {
                Ok(source) => return Ok(source),
                Err(error) => failures.push(format!(
                    "검색 대체 프로필 {} 스트림: {error:#}",
                    profile + 1
                )),
            }
        }
    }
    log::warn!(
        "all YouTube playback profiles failed: {}",
        failures.join(" | ")
    );
    anyhow::bail!("재생 가능한 YouTube 오디오 스트림을 찾지 못했습니다")
}

fn resolve_prefetch_item(
    resolver: &MediaResolver,
    item: &MediaItem,
    format: &str,
) -> Result<ResolvedMedia> {
    if let Some(url) = item.watch_url() {
        return resolver.resolve_profile(&url, format, 0);
    }
    if let Some(candidate) = canonical_collection_candidate(resolver, item)? {
        let url = candidate
            .watch_url()
            .context("원본 컬렉션 후보에 YouTube 영상 ID가 없습니다")?;
        let mut resolved = resolver.resolve_profile(&url, format, 0)?;
        resolved.video_id.clone_from(&candidate.video_id);
        resolved.title = Some(candidate.title);
        return Ok(resolved);
    }
    if let Some(query) = item.fallback_search_query() {
        return resolver.search_profile(&query, format, 0);
    }
    anyhow::bail!("선택한 항목에 재생 영상 또는 YouTube 대체 검색 정보가 없습니다")
}

fn canonical_collection_candidate(
    resolver: &MediaResolver,
    item: &MediaItem,
) -> Result<Option<MediaItem>> {
    let Some((playlist_id, source_index)) = item.canonical_source() else {
        return Ok(None);
    };
    let limit = source_index.saturating_add(25).clamp(50, 100);
    let queue = resolver.playlist_queue(playlist_id, limit)?;
    Ok(select_canonical_candidate(item, &queue.items))
}

fn select_canonical_candidate(item: &MediaItem, candidates: &[MediaItem]) -> Option<MediaItem> {
    candidates
        .iter()
        .filter_map(|candidate| {
            let title_score = title_match_score(&item.title, &candidate.title)?;
            let duration_score =
                duration_match_score(item.duration_seconds, candidate.duration_seconds)?;
            let artist_score = metadata_match_score(&item.subtitle, &candidate.subtitle);
            let same_index = item.source_index == candidate.source_index;
            let duration_missing =
                item.duration_seconds.is_none() || candidate.duration_seconds.is_none();
            if duration_missing && !same_index && artist_score == 0 {
                return None;
            }
            let index_score = u32::from(same_index) * 20;
            Some((
                title_score + duration_score + artist_score + index_score,
                candidate,
            ))
        })
        .max_by_key(|(score, _)| *score)
        .map(|(_, candidate)| candidate.clone())
}

fn title_match_score(expected: &str, candidate: &str) -> Option<u32> {
    let expected = normalized_match_text(expected);
    let candidate = normalized_match_text(candidate);
    if expected.is_empty() || candidate.is_empty() {
        return None;
    }
    if expected == candidate {
        return Some(100);
    }
    let shorter_length = expected.chars().count().min(candidate.chars().count());
    (shorter_length >= 3 && (expected.contains(&candidate) || candidate.contains(&expected)))
        .then_some(70)
}

fn duration_match_score(expected: Option<u64>, candidate: Option<u64>) -> Option<u32> {
    let (Some(expected), Some(candidate)) = (expected, candidate) else {
        return Some(0);
    };
    let tolerance = (expected.saturating_mul(3) / 100).max(8);
    (expected.abs_diff(candidate) <= tolerance).then_some(40)
}

fn metadata_match_score(expected: &str, candidate: &str) -> u32 {
    let expected = metadata_tokens(expected);
    let candidate = metadata_tokens(candidate);
    if expected
        .iter()
        .any(|left| candidate.iter().any(|right| left == right))
    {
        20
    } else {
        0
    }
}

fn metadata_tokens(value: &str) -> Vec<String> {
    value
        .split(['·', ',', ';', '/', '|', '&'])
        .map(normalized_match_text)
        .filter(|token| token.chars().count() >= 2 && token != "youtube")
        .collect()
}

fn normalized_match_text(value: &str) -> String {
    value
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|character| character.is_alphanumeric())
        .collect()
}

fn open_resolved_source(
    runtime: &tokio::runtime::Runtime,
    config: &AppConfig,
    paths: &CachePaths,
    resolved: &ResolvedMedia,
    start_position: Duration,
) -> Result<OpenedSource> {
    let resolved_duration = resolved
        .duration_seconds
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .map(Duration::from_secs_f64);
    let stderr_file =
        tempfile::NamedTempFile::new().context("오디오 오류 로그를 만들지 못했습니다")?;
    let stderr_handle = stderr_file
        .as_file()
        .try_clone()
        .context("오디오 오류 로그를 복제하지 못했습니다")?;
    let helper = media_helper_command(
        config,
        resolved,
        &paths.audio,
        &paths.duration,
        &paths.buffered,
        start_position,
    )?;
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
                .retry_timeout(Duration::from_secs(30))
                // Dropping the active decoder on seek must not discard the already
                // started session segment. The download continues into its cache,
                // while playback switches to the newly requested segment.
                .cancel_on_drop(false),
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
            anyhow::bail!("오디오를 스트리밍하지 못했습니다: {concise}");
        }
    };
    let replacement = resolved
        .title
        .as_deref()
        .zip(resolved.video_id.as_deref())
        .filter(|(title, video_id)| !title.is_empty() && !video_id.is_empty())
        .map(|(title, video_id)| PlaybackReplacement {
            title: title.to_owned(),
            video_id: video_id.to_owned(),
        });
    if let Some(replacement) = &replacement
        && let Ok(serialized) = serde_json::to_vec(replacement)
        && let Err(error) = std::fs::write(&paths.replacement, serialized)
    {
        log::debug!("대체 영상 메타데이터를 캐시하지 못했습니다: {error}");
    }
    Ok((
        Box::new(decoder),
        resolved_duration.or_else(|| read_duration(&paths.duration)),
        replacement,
    ))
}

fn read_duration(path: &Path) -> Option<Duration> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|value| value.lines().last()?.trim().parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .map(Duration::from_secs_f64)
}

fn read_replacement(path: &Path) -> Option<PlaybackReplacement> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn media_helper_command(
    config: &AppConfig,
    media: &ResolvedMedia,
    cache_path: &Path,
    duration_path: &Path,
    buffered_path: &Path,
    start_position: Duration,
) -> Result<StdCommand> {
    let executable = std::env::current_exe().context("현재 앱 실행 파일을 찾지 못했습니다")?;
    let encoded_headers = BASE64.encode(
        serde_json::to_vec(&media.headers).context("오디오 요청 헤더를 직렬화하지 못했습니다")?,
    );
    let mut command = StdCommand::new(executable);
    command
        .arg(MEDIA_STREAM_FLAG)
        .arg("--ffmpeg")
        .arg(&config.ffmpeg)
        .arg("--media-url")
        .arg(&media.url)
        .arg("--headers")
        .arg(encoded_headers)
        .arg("--cache")
        .arg(cache_path)
        .arg("--duration")
        .arg(duration_path)
        .arg("--buffered")
        .arg(buffered_path);
    if !start_position.is_zero() {
        command
            .arg("--start-seconds")
            .arg(start_position.as_secs_f64().to_string());
    }
    if let Some(duration) = media.duration_seconds {
        command.arg("--media-duration").arg(duration.to_string());
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
    let segment_start = args.start_seconds.unwrap_or(0.0);
    publish_buffered_progress(
        &args.buffered,
        BufferedProgress {
            start_seconds: segment_start,
            end_seconds: segment_start,
            complete: false,
        },
    );
    if let Some(duration) = args.media_duration {
        std::fs::write(&duration_part, format!("{duration}\n"))
            .context("오디오 길이 메타데이터를 저장하지 못했습니다")?;
    }

    let mut ffmpeg_command = StdCommand::new(&args.ffmpeg);
    ffmpeg_command.args(["-i", "pipe:"]);
    if let Some(start_seconds) = args.start_seconds {
        ffmpeg_command.arg("-ss").arg(start_seconds.to_string());
    }
    let mut ffmpeg = ffmpeg_command
        .args([
            "-map",
            "a",
            "-f",
            "wav",
            "-loglevel",
            "error",
            "-vn",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-acodec",
            "pcm_s16le",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("'{}' 실행 파일을 시작하지 못했습니다", args.ffmpeg))?;
    let ffmpeg_stdin = ffmpeg
        .stdin
        .take()
        .context("ffmpeg 오디오 입력을 열지 못했습니다")?;
    let media_url = args.media_url.clone();
    let media_headers = args.headers.clone();
    let download = thread::Builder::new()
        .name("pocket-ytm-range-download".into())
        .spawn(move || stream_http_ranges(&media_url, &media_headers, ffmpeg_stdin))
        .context("Rust 오디오 다운로드 스레드를 시작하지 못했습니다")?;
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
            publish_buffered_progress(
                &args.buffered,
                buffered_progress_from_output(segment_start, written, args.media_duration, false),
            );
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
    }
    let ffmpeg_status = ffmpeg
        .wait()
        .context("ffmpeg 종료 상태를 읽지 못했습니다")?;
    let download_result = download
        .join()
        .map_err(|_| anyhow::anyhow!("Rust 오디오 다운로드 스레드가 중단되었습니다"))?;
    publish_duration(&duration_part, &args.duration);

    if let Err(error) = copy_result {
        let _ = std::fs::remove_file(&cache_part);
        let _ = std::fs::remove_file(&duration_part);
        return Err(error);
    }
    if let Err(error) = download_result {
        let _ = std::fs::remove_file(&cache_part);
        let _ = std::fs::remove_file(&duration_part);
        return Err(error);
    }
    if !ffmpeg_status.success() || written <= WAV_HEADER_ESTIMATE {
        let _ = std::fs::remove_file(&cache_part);
        let _ = std::fs::remove_file(&duration_part);
        anyhow::bail!("오디오 파이프라인이 완료되지 않았습니다 (ffmpeg: {ffmpeg_status})");
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
    publish_buffered_progress(
        &args.buffered,
        buffered_progress_from_output(segment_start, written, args.media_duration, true),
    );
    std::fs::remove_file(&duration_part).ok();
    Ok(())
}

fn stream_http_ranges(
    url: &str,
    headers: &std::collections::BTreeMap<String, String>,
    mut output: impl std::io::Write,
) -> Result<u64> {
    let client = BlockingClient::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .context("Rust 오디오 HTTP 클라이언트를 만들지 못했습니다")?;
    let mut request_headers = HeaderMap::new();
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .with_context(|| format!("오디오 요청 헤더 이름이 올바르지 않습니다: {name}"))?;
        if name == RANGE || name == ACCEPT_ENCODING {
            continue;
        }
        let value = HeaderValue::from_str(value)
            .with_context(|| format!("오디오 요청 헤더 값이 올바르지 않습니다: {name}"))?;
        request_headers.insert(name, value);
    }

    let mut offset = 0_u64;
    let mut consecutive_failures = 0_usize;
    loop {
        let end = offset.saturating_add(HTTP_RANGE_CHUNK_BYTES - 1);
        let mut response = match client
            .get(url)
            .headers(request_headers.clone())
            .header(ACCEPT_ENCODING, "identity")
            .header(RANGE, format!("bytes={offset}-{end}"))
            .send()
        {
            Ok(response) => response,
            Err(error) => {
                wait_for_range_retry(
                    &mut consecutive_failures,
                    format!("오디오 Range 요청을 보내지 못했습니다: {error}"),
                )?;
                continue;
            }
        };

        if offset == 0 && response.status() == StatusCode::OK {
            return std::io::copy(&mut response, &mut output)
                .context("오디오 응답을 FFmpeg에 전달하지 못했습니다");
        }
        if response.status() == StatusCode::FORBIDDEN {
            anyhow::bail!(
                "오디오 서버가 Range 요청을 거부했습니다: {}",
                response.status()
            );
        }
        if response.status() != StatusCode::PARTIAL_CONTENT {
            wait_for_range_retry(
                &mut consecutive_failures,
                format!(
                    "오디오 서버가 Range 요청을 거부했습니다: {}",
                    response.status()
                ),
            )?;
            continue;
        }

        let content_range = response
            .headers()
            .get(CONTENT_RANGE)
            .context("오디오 Range 응답에 Content-Range가 없습니다")?
            .to_str()
            .context("오디오 Content-Range가 올바르지 않습니다")?;
        let (actual_start, actual_end, total) = parse_content_range(content_range)?;
        if actual_start != offset || actual_end < actual_start {
            anyhow::bail!(
                "오디오 Range 순서가 올바르지 않습니다: expected {offset}, got {actual_start}-{actual_end}"
            );
        }
        let expected = actual_end - actual_start + 1;
        let mut remaining = expected;
        let mut buffer = [0_u8; 64 * 1024];
        let mut read_failure = None;
        while remaining > 0 {
            let limit = usize::try_from(remaining.min(buffer.len() as u64))
                .expect("Range read size fits usize");
            match response.read(&mut buffer[..limit]) {
                Ok(0) => {
                    read_failure = Some(format!(
                        "오디오 Range가 중간에 끝났습니다: {remaining} bytes remaining"
                    ));
                    break;
                }
                Ok(count) => {
                    output
                        .write_all(&buffer[..count])
                        .context("오디오 Range를 FFmpeg에 전달하지 못했습니다")?;
                    offset += count as u64;
                    remaining -= count as u64;
                }
                Err(error) => {
                    read_failure = Some(format!("오디오 Range를 읽지 못했습니다: {error}"));
                    break;
                }
            }
        }
        if let Some(error) = read_failure {
            wait_for_range_retry(&mut consecutive_failures, error)?;
            continue;
        }
        consecutive_failures = 0;
        if offset >= total {
            return Ok(offset);
        }
    }
}

fn wait_for_range_retry(consecutive_failures: &mut usize, error: String) -> Result<()> {
    *consecutive_failures += 1;
    if *consecutive_failures >= HTTP_RANGE_MAX_RETRIES {
        anyhow::bail!("{error} ({HTTP_RANGE_MAX_RETRIES}회 재시도 실패)");
    }
    thread::sleep(Duration::from_millis(150 * *consecutive_failures as u64));
    Ok(())
}

fn parse_content_range(value: &str) -> Result<(u64, u64, u64)> {
    let value = value
        .strip_prefix("bytes ")
        .context("Content-Range 단위가 bytes가 아닙니다")?;
    let (range, total) = value
        .split_once('/')
        .context("Content-Range 전체 길이가 없습니다")?;
    let (start, end) = range
        .split_once('-')
        .context("Content-Range 범위가 없습니다")?;
    let start = start
        .parse()
        .context("Content-Range 시작점이 올바르지 않습니다")?;
    let end = end
        .parse()
        .context("Content-Range 끝점이 올바르지 않습니다")?;
    let total = total
        .parse()
        .context("Content-Range 전체 길이가 올바르지 않습니다")?;
    Ok((start, end, total))
}

#[derive(Debug)]
struct MediaArgs {
    ffmpeg: String,
    media_url: String,
    headers: std::collections::BTreeMap<String, String>,
    media_duration: Option<f64>,
    start_seconds: Option<f64>,
    cache: PathBuf,
    duration: PathBuf,
    buffered: PathBuf,
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
    let encoded_headers = required("--headers")?;
    let headers = BASE64
        .decode(encoded_headers.to_string_lossy().as_bytes())
        .context("오디오 요청 헤더를 디코딩하지 못했습니다")?;
    let headers =
        serde_json::from_slice(&headers).context("오디오 요청 헤더 형식이 올바르지 않습니다")?;
    let media_duration = values
        .get("--media-duration")
        .map(|value| value.to_string_lossy().parse::<f64>())
        .transpose()
        .context("오디오 길이가 올바르지 않습니다")?
        .filter(|duration| duration.is_finite() && *duration > 0.0);
    let start_seconds = values
        .get("--start-seconds")
        .map(|value| value.to_string_lossy().parse::<f64>())
        .transpose()
        .context("탐색 시작 위치가 올바르지 않습니다")?
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0);
    Ok(MediaArgs {
        ffmpeg: required("--ffmpeg")?.to_string_lossy().into_owned(),
        media_url: required("--media-url")?.to_string_lossy().into_owned(),
        headers,
        media_duration,
        start_seconds,
        cache: PathBuf::from(required("--cache")?),
        duration: PathBuf::from(required("--duration")?),
        buffered: PathBuf::from(required("--buffered")?),
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

fn buffered_progress_from_output(
    start_seconds: f64,
    written: u64,
    media_duration: Option<f64>,
    complete: bool,
) -> BufferedProgress {
    let pcm_bytes_per_second =
        NORMALIZED_SAMPLE_RATE * NORMALIZED_CHANNELS * NORMALIZED_BYTES_PER_SAMPLE;
    let decoded_seconds =
        written.saturating_sub(WAV_HEADER_ESTIMATE) as f64 / pcm_bytes_per_second as f64;
    let measured_end = start_seconds + decoded_seconds;
    let end_seconds = if complete {
        media_duration.unwrap_or(measured_end)
    } else {
        media_duration
            .map(|duration| measured_end.min(duration))
            .unwrap_or(measured_end)
    };
    BufferedProgress {
        start_seconds,
        end_seconds: end_seconds.max(start_seconds),
        complete,
    }
}

fn publish_buffered_progress(path: &Path, progress: BufferedProgress) {
    let Ok(serialized) = serde_json::to_vec(&progress) else {
        return;
    };
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    if std::fs::write(&temporary, serialized).is_err() {
        return;
    }
    if path.is_file() {
        std::fs::remove_file(path).ok();
    }
    if std::fs::rename(&temporary, path).is_err() {
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
        let encoded_headers = BASE64.encode(br#"{"User-Agent":"Pocket Test"}"#);
        let args = vec![
            OsString::from("--ffmpeg"),
            OsString::from("ffmpeg"),
            OsString::from("--media-url"),
            OsString::from("https://media.example/audio"),
            OsString::from("--headers"),
            OsString::from(encoded_headers),
            OsString::from("--media-duration"),
            OsString::from("42.5"),
            OsString::from("--start-seconds"),
            OsString::from("12.25"),
            OsString::from("--cache"),
            OsString::from("/tmp/cache.wav"),
            OsString::from("--duration"),
            OsString::from("/tmp/cache.duration"),
            OsString::from("--buffered"),
            OsString::from("/tmp/cache.buffered.json"),
        ]
        .into_iter();
        let parsed = parse_media_args(args).unwrap();
        assert_eq!(parsed.cache, Path::new("/tmp/cache.wav"));
        assert_eq!(parsed.duration, Path::new("/tmp/cache.duration"));
        assert_eq!(parsed.buffered, Path::new("/tmp/cache.buffered.json"));
        assert_eq!(parsed.media_duration, Some(42.5));
        assert_eq!(parsed.start_seconds, Some(12.25));
        assert_eq!(parsed.headers.get("User-Agent").unwrap(), "Pocket Test");
    }

    #[test]
    fn buffered_ranges_preserve_disjoint_seek_segments() {
        let root = tempfile::tempdir().unwrap();
        let item = item("abc");
        let first = cache_paths(root.path(), &item);
        let second = seek_cache_paths(root.path(), &item, 7);
        publish_buffered_progress(
            &first.buffered,
            BufferedProgress {
                start_seconds: 0.0,
                end_seconds: 42.0,
                complete: false,
            },
        );
        publish_buffered_progress(
            &second.buffered,
            BufferedProgress {
                start_seconds: 120.0,
                end_seconds: 148.0,
                complete: false,
            },
        );

        assert_eq!(
            merged_buffered_ranges(root.path(), &item),
            vec![
                BufferedRange {
                    start: Duration::ZERO,
                    end: Duration::from_secs(42),
                },
                BufferedRange {
                    start: Duration::from_secs(120),
                    end: Duration::from_secs(148),
                },
            ]
        );
    }

    #[test]
    fn normalized_pcm_bytes_map_to_buffered_seconds() {
        let one_second = NORMALIZED_SAMPLE_RATE * NORMALIZED_CHANNELS * NORMALIZED_BYTES_PER_SAMPLE;
        let progress = buffered_progress_from_output(
            30.0,
            WAV_HEADER_ESTIMATE + one_second * 5,
            Some(100.0),
            false,
        );
        assert_eq!(progress.start_seconds, 30.0);
        assert_eq!(progress.end_seconds, 35.0);
        assert!(!progress.complete);
    }

    #[test]
    fn parses_http_content_ranges() {
        assert_eq!(
            parse_content_range("bytes 2097152-3433754/3433755").unwrap(),
            (2097152, 3433754, 3433755)
        );
        assert!(parse_content_range("items 0-1/2").is_err());
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

    #[test]
    fn canonical_match_accepts_the_official_track_from_its_source_collection() {
        let catalog = MediaItem {
            kind: "song".into(),
            title: "きっとね！ - maybe!".into(),
            subtitle: "Eye · Muto · SEE · maybe!".into(),
            source_playlist_id: Some("OLAK5uy_exact".into()),
            source_index: Some(0),
            duration_seconds: Some(250),
            ..Default::default()
        };
        let official = MediaItem {
            title: "きっとね！".into(),
            subtitle: "Eye".into(),
            video_id: Some("zLwC-6o4OW4".into()),
            source_index: Some(0),
            duration_seconds: Some(250),
            ..Default::default()
        };

        assert_eq!(
            select_canonical_candidate(&catalog, std::slice::from_ref(&official)),
            Some(official)
        );
    }

    #[test]
    fn canonical_match_recovers_from_a_shifted_collection_index() {
        let catalog = MediaItem {
            title: "Target Song".into(),
            subtitle: "Target Artist · Album".into(),
            source_index: Some(4),
            duration_seconds: Some(181),
            ..Default::default()
        };
        let stale_index = MediaItem {
            title: "Different Song".into(),
            subtitle: "Other Artist".into(),
            video_id: Some("wrong".into()),
            source_index: Some(4),
            duration_seconds: Some(181),
            ..Default::default()
        };
        let shifted_match = MediaItem {
            title: "Target Song".into(),
            subtitle: "Target Artist".into(),
            video_id: Some("correct".into()),
            source_index: Some(5),
            duration_seconds: Some(182),
            ..Default::default()
        };

        assert_eq!(
            select_canonical_candidate(&catalog, &[stale_index, shifted_match.clone()]),
            Some(shifted_match)
        );
    }

    #[test]
    fn canonical_match_rejects_a_different_version_with_the_wrong_duration() {
        let catalog = MediaItem {
            title: "Target Song".into(),
            source_index: Some(0),
            duration_seconds: Some(180),
            ..Default::default()
        };
        let live_version = MediaItem {
            title: "Target Song (Live)".into(),
            video_id: Some("live".into()),
            source_index: Some(0),
            duration_seconds: Some(320),
            ..Default::default()
        };

        assert_eq!(select_canonical_candidate(&catalog, &[live_version]), None);
    }
}
