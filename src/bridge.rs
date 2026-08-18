use std::{
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context as _, Result, anyhow, bail};
use parking_lot::Mutex;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{
    config::AppConfig,
    model::{AccountStatus, BrowsePage, Lyrics, MediaItem, MediaSection, WatchQueue},
};

pub struct YtMusicBridge {
    config: AppConfig,
    script: PathBuf,
    process: Mutex<Option<Sidecar>>,
    next_id: AtomicU64,
}

struct Sidecar {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

#[derive(Deserialize)]
struct Envelope {
    id: u64,
    ok: bool,
    #[serde(default)]
    data: Value,
    #[serde(default)]
    error: String,
}

impl YtMusicBridge {
    pub fn new(config: AppConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            script: bridge_script_path(),
            process: Mutex::new(None),
            next_id: AtomicU64::new(1),
        })
    }

    pub fn home(&self) -> Result<Vec<MediaSection>> {
        self.request("home", json!({"limit": 8}))
    }

    pub fn auth_status(&self) -> Result<AccountStatus> {
        self.request("authStatus", json!({}))
    }

    pub fn authenticate(&self, headers: &str) -> Result<AccountStatus> {
        self.request("authenticate", json!({"headers": headers}))
    }

    pub fn logout(&self) -> Result<AccountStatus> {
        self.request("logout", json!({}))
    }

    pub fn explore(&self) -> Result<Vec<MediaSection>> {
        self.request("explore", json!({}))
    }

    pub fn search(&self, query: &str) -> Result<Vec<MediaItem>> {
        self.request("search", json!({"query": query, "limit": 40}))
    }

    pub fn library(&self, category: &str) -> Result<Vec<MediaSection>> {
        self.request("library", json!({"category": category, "limit": 100}))
    }

    pub fn browse(&self, item: &MediaItem) -> Result<BrowsePage> {
        self.request(
            "browse",
            json!({
                "kind": item.kind,
                "browseId": item.browse_id,
                "playlistId": item.playlist_id,
            }),
        )
    }

    pub fn watch_queue(&self, video_id: &str) -> Result<WatchQueue> {
        self.request("watch", json!({"videoId": video_id, "limit": 50}))
    }

    pub fn playlist_queue(&self, playlist_id: &str) -> Result<WatchQueue> {
        self.request(
            "playlistQueue",
            json!({"playlistId": playlist_id, "limit": 50}),
        )
    }

    pub fn lyrics(&self, browse_id: &str) -> Result<Lyrics> {
        self.request("lyrics", json!({"browseId": browse_id}))
    }

    pub fn rate_song(&self, video_id: &str, rating: &str) -> Result<Value> {
        self.request("rateSong", json!({"videoId": video_id, "rating": rating}))
    }

    fn request<T: DeserializeOwned>(&self, op: &str, params: Value) -> Result<T> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = json!({"id": id, "op": op, "params": params});
        let mut guard = self.process.lock();

        for attempt in 0..2 {
            if guard.is_none() {
                *guard = Some(
                    self.spawn()
                        .context("ytmusicapi 사이드카를 시작하지 못했습니다")?,
                );
            }

            let result = guard
                .as_mut()
                .expect("sidecar initialized")
                .round_trip(id, &request);
            match result {
                Ok(envelope) => {
                    if !envelope.ok {
                        bail!(envelope.error);
                    }
                    return serde_json::from_value(envelope.data)
                        .context("ytmusicapi 응답 형식이 올바르지 않습니다");
                }
                Err(error) if attempt == 0 => {
                    log::warn!("restarting ytmusic sidecar after error: {error:#}");
                    *guard = None;
                }
                Err(error) => return Err(error),
            }
        }

        Err(anyhow!("ytmusicapi 요청에 실패했습니다"))
    }

    fn spawn(&self) -> Result<Sidecar> {
        if !self.script.exists() {
            bail!("Python bridge not found: {}", self.script.display());
        }

        let mut command = Command::new(&self.config.python);
        command
            .arg("-u")
            .arg(&self.script)
            .arg("--language")
            .arg(&self.config.language)
            .arg("--location")
            .arg(&self.config.location)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.arg("--auth").arg(&self.config.auth_path);

        let mut child = command.spawn().with_context(|| {
            format!(
                "Python 실행 파일 '{}'을(를) 시작할 수 없습니다",
                self.config.python
            )
        })?;
        let stdin = child.stdin.take().context("sidecar stdin unavailable")?;
        let stdout = BufReader::new(child.stdout.take().context("sidecar stdout unavailable")?);
        Ok(Sidecar {
            child,
            stdin,
            stdout,
        })
    }
}

fn bridge_script_path() -> PathBuf {
    if let Some(path) = std::env::var_os("POCKET_YTM_BRIDGE") {
        return PathBuf::from(path);
    }
    if let Ok(executable) = std::env::current_exe()
        && let Some(macos_dir) = executable.parent()
    {
        let bundled = macos_dir
            .parent()
            .map(|contents| contents.join("Resources/backend/ytmusic_bridge.py"));
        if let Some(path) = bundled.filter(|path| path.exists()) {
            return path;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("backend")
        .join("ytmusic_bridge.py")
}

impl Sidecar {
    fn round_trip(&mut self, expected_id: u64, request: &Value) -> Result<Envelope> {
        serde_json::to_writer(&mut self.stdin, request)?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;

        let mut line = String::new();
        let count = self.stdout.read_line(&mut line)?;
        if count == 0 {
            let status = self.child.try_wait()?.map(|status| status.to_string());
            let mut stderr = String::new();
            if let Some(mut pipe) = self.child.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            bail!(
                "ytmusicapi sidecar exited{}: {}",
                status
                    .map(|value| format!(" ({value})"))
                    .unwrap_or_default(),
                stderr.trim()
            );
        }

        let envelope: Envelope = serde_json::from_str(&line)
            .with_context(|| format!("invalid sidecar response: {}", line.trim()))?;
        if envelope.id != expected_id {
            bail!(
                "sidecar response id mismatch: expected {expected_id}, got {}",
                envelope.id
            );
        }
        Ok(envelope)
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
