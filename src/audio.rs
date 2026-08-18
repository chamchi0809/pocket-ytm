use std::{
    io::Read as _,
    process::Stdio,
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

use anyhow::{Context as _, Result};
use parking_lot::RwLock;
use rodio::{Decoder, DeviceSinkBuilder, Player};
use stream_download::{
    Settings, StreamDownload,
    process::{CommandBuilder, FfmpegConvertAudioCommand, ProcessStreamParams, YtDlpCommand},
    storage::temp::TempStorageProvider,
};

use crate::{config::AppConfig, model::MediaItem};

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
}

enum AudioCommand {
    Load(MediaItem),
    Toggle,
    Seek(Duration),
    SetVolume(f32),
}

impl AudioEngine {
    pub fn new(config: AppConfig) -> Self {
        let (tx, rx) = mpsc::channel();
        let snapshot = Arc::new(RwLock::new(AudioSnapshot::default()));
        let thread_snapshot = snapshot.clone();
        thread::Builder::new()
            .name("pocket-ytm-audio".into())
            .spawn(move || audio_loop(config, rx, thread_snapshot))
            .expect("failed to start native audio thread");
        Self { tx, snapshot }
    }

    pub fn snapshot(&self) -> AudioSnapshot {
        self.snapshot.read().clone()
    }

    pub fn load(&self, item: MediaItem) {
        let _ = self.tx.send(AudioCommand::Load(item));
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

fn audio_loop(
    config: AppConfig,
    rx: mpsc::Receiver<AudioCommand>,
    snapshot: Arc<RwLock<AudioSnapshot>>,
) {
    let device = match DeviceSinkBuilder::open_default_sink() {
        Ok(device) => device,
        Err(error) => {
            set_error(
                &snapshot,
                format!("오디오 출력 장치를 열 수 없습니다: {error}"),
            );
            return;
        }
    };
    let player = Player::connect_new(device.mixer());
    player.set_volume(snapshot.read().volume);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            set_error(
                &snapshot,
                format!("오디오 런타임을 시작할 수 없습니다: {error}"),
            );
            return;
        }
    };

    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(AudioCommand::Load(item)) => {
                player.stop();
                {
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
                }

                match open_source(&runtime, &config, &item) {
                    Ok((source, decoded_duration)) => {
                        player.append(source);
                        player.set_volume(snapshot.read().volume);
                        player.play();
                        let mut state = snapshot.write();
                        if state.duration.is_zero()
                            && let Some(duration) = decoded_duration
                        {
                            state.duration = duration;
                        }
                        state.phase = PlaybackPhase::Playing;
                    }
                    Err(error) => set_error(
                        &snapshot,
                        format!("재생 스트림을 열 수 없습니다: {error:#}"),
                    ),
                }
            }
            Ok(AudioCommand::Toggle) => {
                let mut state = snapshot.write();
                match state.phase {
                    PlaybackPhase::Playing => {
                        player.pause();
                        state.phase = PlaybackPhase::Paused;
                    }
                    PlaybackPhase::Paused => {
                        player.play();
                        state.phase = PlaybackPhase::Playing;
                    }
                    _ => {}
                }
            }
            Ok(AudioCommand::Seek(position)) => {
                if let Err(error) = player.try_seek(position) {
                    snapshot.write().error = Some(format!("탐색할 수 없습니다: {error}"));
                } else {
                    snapshot.write().position = position;
                }
            }
            Ok(AudioCommand::SetVolume(volume)) => {
                player.set_volume(volume);
                snapshot.write().volume = volume;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        let mut state = snapshot.write();
        if matches!(state.phase, PlaybackPhase::Playing | PlaybackPhase::Paused) {
            state.position = player.get_pos();
            if player.empty() {
                state.phase = PlaybackPhase::Ended;
                if state.duration.is_zero() {
                    state.duration = state.position;
                }
            }
        }
    }
}

type NativeSource = Decoder<
    Box<stream_download::StreamDownload<stream_download::storage::temp::TempStorageProvider>>,
>;

fn open_source(
    runtime: &tokio::runtime::Runtime,
    config: &AppConfig,
    item: &MediaItem,
) -> Result<(NativeSource, Option<Duration>)> {
    let url = item
        .watch_url()
        .context("선택한 항목에 YouTube videoId가 없습니다")?;

    open_source_with_format(runtime, config, &url, "bestaudio/best")
}

fn open_source_with_format(
    runtime: &tokio::runtime::Runtime,
    config: &AppConfig,
    url: &str,
    format: &str,
) -> Result<(NativeSource, Option<Duration>)> {
    let stderr_file =
        tempfile::NamedTempFile::new().context("오디오 오류 로그를 만들지 못했습니다")?;
    let duration_file = tempfile::NamedTempFile::new()
        .context("오디오 길이 메타데이터 파일을 만들지 못했습니다")?;
    let stderr_handle = stderr_file
        .as_file()
        .try_clone()
        .context("오디오 오류 로그를 복제하지 못했습니다")?;
    let command = YtDlpCommand::new(url)
        .yt_dlp_path(&config.yt_dlp)
        .format(format)
        .into_command()
        .arg("--no-playlist")
        .arg("--print-to-file")
        .arg("before_dl:%(duration)s")
        .arg(duration_file.path())
        .stderr_handle(Stdio::from(stderr_handle));
    let command = if let Some(cookies) = &config.cookies_path
        && cookies.exists()
    {
        command.arg("--cookies").arg(cookies)
    } else {
        command
    };
    let pipeline = CommandBuilder::new(command).pipe(
        FfmpegConvertAudioCommand::new("wav")
            .ffmpeg_path(&config.ffmpeg)
            .args(["-vn", "-acodec", "pcm_s16le"]),
    );

    let yt_dlp_name = config.yt_dlp.clone();
    let reader = runtime.block_on(async move {
        let params = ProcessStreamParams::new(pipeline).with_context(|| {
            format!(
                "'{yt_dlp_name}' 실행 파일을 시작하지 못했습니다. yt-dlp 설치 상태를 확인하세요"
            )
        })?;
        let reader = StreamDownload::new_process(
            params,
            TempStorageProvider::new(),
            Settings::default()
                .prefetch_bytes(1024 * 512)
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
    let duration = std::fs::read_to_string(duration_file.path())
        .ok()
        .and_then(|value| value.lines().last()?.trim().parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .map(Duration::from_secs_f64);
    Ok((decoder, duration))
}

fn set_error(snapshot: &RwLock<AudioSnapshot>, error: String) {
    let mut state = snapshot.write();
    state.phase = PlaybackPhase::Error;
    state.error = Some(error);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires YouTube network access and yt-dlp"]
    fn yt_dlp_stream_reaches_native_decoder() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let config = AppConfig::from_env();
        let item = MediaItem {
            id: "jNQXAC9IVRw".into(),
            title: "audio smoke test".into(),
            video_id: Some("jNQXAC9IVRw".into()),
            ..Default::default()
        };

        let (_, duration) = open_source(&runtime, &config, &item)
            .expect("yt-dlp stream should reach the native rodio decoder");
        assert!(
            duration.is_some_and(|duration| duration < Duration::from_secs(60)),
            "streaming WAV sentinel length must not replace the real video duration: {duration:?}"
        );
        open_source_with_format(
            &runtime,
            &config,
            item.watch_url().as_deref().unwrap(),
            "bestaudio[ext=webm]/bestaudio",
        )
        .expect("ffmpeg should normalize WebM audio for the native decoder");

        let unavailable = MediaItem {
            id: "00000000000".into(),
            video_id: Some("00000000000".into()),
            ..Default::default()
        };
        let error = match open_source(&runtime, &config, &unavailable) {
            Ok(_) => panic!("an unavailable video must not produce an audio source"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("yt-dlp가 오디오를 내려받지 못했습니다"),
            "the player should surface the extractor error: {error:#}"
        );
    }
}
