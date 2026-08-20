use std::{
    collections::HashMap,
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
    entrypoint: PathBuf,
    process: Mutex<Option<Sidecar>>,
    query_cache: Mutex<HashMap<String, Value>>,
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
            entrypoint: bridge_entrypoint_path(),
            process: Mutex::new(None),
            query_cache: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        })
    }

    pub fn home(&self) -> Result<Vec<MediaSection>> {
        self.query("home", json!({"limit": 8}))
    }

    pub fn cached_auth_status(&self) -> AccountStatus {
        cached_auth_status(&self.config.auth_path)
    }

    pub fn invalidate_query_cache(&self) {
        self.query_cache.lock().clear();
    }

    pub fn authenticate(&self, headers: &str) -> Result<AccountStatus> {
        self.mutation("authenticate", json!({"headers": headers}))
    }

    pub fn quick_login(&self) -> Result<AccountStatus> {
        self.mutation("quickLogin", json!({}))
    }

    pub fn logout(&self) -> Result<AccountStatus> {
        self.mutation("logout", json!({}))
    }

    pub fn explore(&self) -> Result<Vec<MediaSection>> {
        self.query("explore", json!({}))
    }

    pub fn search(&self, query: &str) -> Result<Vec<MediaItem>> {
        self.query("search", json!({"query": query, "limit": 40}))
    }

    pub fn library(&self, category: &str) -> Result<Vec<MediaSection>> {
        self.query("library", json!({"category": category, "limit": 100}))
    }

    pub fn browse(&self, item: &MediaItem) -> Result<BrowsePage> {
        self.query(
            "browse",
            json!({
                "kind": item.kind,
                "browseId": item.browse_id,
                "playlistId": item.playlist_id,
            }),
        )
    }

    pub fn watch_queue(&self, video_id: &str) -> Result<WatchQueue> {
        self.query("watch", json!({"videoId": video_id, "limit": 50}))
    }

    pub fn playlist_queue(&self, playlist_id: &str) -> Result<WatchQueue> {
        self.query(
            "playlistQueue",
            json!({"playlistId": playlist_id, "limit": 50}),
        )
    }

    pub fn lyrics(&self, browse_id: &str) -> Result<Lyrics> {
        self.query("lyrics", json!({"browseId": browse_id}))
    }

    pub fn rate_song(&self, video_id: &str, rating: &str) -> Result<Value> {
        self.mutation("rateSong", json!({"videoId": video_id, "rating": rating}))
    }

    fn query<T: DeserializeOwned>(&self, op: &str, params: Value) -> Result<T> {
        self.request(op, params, true)
    }

    fn mutation<T: DeserializeOwned>(&self, op: &str, params: Value) -> Result<T> {
        self.request(op, params, false)
    }

    fn request<T: DeserializeOwned>(&self, op: &str, params: Value, cacheable: bool) -> Result<T> {
        let cache_key = cacheable.then(|| query_key(op, &params));
        // The process lock also acts as an in-flight query lock. Once one query
        // finishes, another caller with the same key observes its cached value.
        let mut guard = self.process.lock();
        if let Some(cache_key) = &cache_key
            && let Some(cached) = self.query_cache.lock().get(cache_key).cloned()
        {
            return serde_json::from_value(cached)
                .context("캐시된 ytmusicapi 응답 형식이 올바르지 않습니다");
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = json!({"id": id, "op": op, "params": params});

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
                    let parsed = serde_json::from_value(envelope.data.clone())
                        .context("ytmusicapi 응답 형식이 올바르지 않습니다")?;
                    let mut cache = self.query_cache.lock();
                    if let Some(cache_key) = &cache_key {
                        cache.insert(cache_key.clone(), envelope.data);
                    } else {
                        cache.clear();
                    }
                    return Ok(parsed);
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
        if !self.entrypoint.exists() {
            bail!("ytmusicapi bridge not found: {}", self.entrypoint.display());
        }

        let is_python_script = self
            .entrypoint
            .extension()
            .is_some_and(|value| value == "py");
        let mut command = if is_python_script {
            let mut command = Command::new(&self.config.python);
            command.arg("-u").arg(&self.entrypoint);
            command
        } else {
            Command::new(&self.entrypoint)
        };
        command
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
                "ytmusicapi 브리지 '{}'을(를) 시작할 수 없습니다",
                self.entrypoint.display()
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

fn cached_auth_status(auth_path: &std::path::Path) -> AccountStatus {
    if !auth_path.is_file() {
        return AccountStatus::default();
    }
    let account_path = auth_path.with_file_name("account.json");
    let account = std::fs::read_to_string(account_path)
        .ok()
        .and_then(|contents| serde_json::from_str::<Value>(&contents).ok())
        .unwrap_or_default();
    let text = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| account.get(key).and_then(Value::as_str))
            .unwrap_or_default()
            .to_owned()
    };
    let thumbnail = text(&["accountPhotoUrl", "thumbnail"]);
    AccountStatus {
        authenticated: true,
        name: text(&["accountName", "name"]),
        handle: text(&["channelHandle", "handle"]),
        thumbnail: (!thumbnail.is_empty()).then_some(thumbnail),
    }
}

fn query_key(op: &str, params: &Value) -> String {
    let params = canonical_json(params);
    serde_json::to_string(&(op, params)).expect("serializing a JSON query key cannot fail")
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            let mut canonical = serde_json::Map::new();
            for key in keys {
                canonical.insert(key.clone(), canonical_json(&object[key]));
            }
            Value::Object(canonical)
        }
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        _ => value.clone(),
    }
}

fn bridge_entrypoint_path() -> PathBuf {
    if let Some(path) = std::env::var_os("POCKET_YTM_BRIDGE") {
        return PathBuf::from(path);
    }
    if let Some(path) = crate::config::bundled_tool_path("pocket-ytm-bridge") {
        return path;
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn query_keys_include_operation_and_parameters() {
        let first = query_key("search", &json!({"query": "cover", "limit": 40}));
        let same = query_key("search", &json!({"limit": 40, "query": "cover"}));
        let different = query_key("search", &json!({"query": "nightcore", "limit": 40}));

        assert_eq!(first, same);
        assert_ne!(first, different);
        assert_ne!(
            first,
            query_key("browse", &json!({"query": "cover", "limit": 40}))
        );
    }

    #[test]
    fn cached_account_restores_without_starting_the_sidecar() {
        let directory = tempdir().unwrap();
        let auth_path = directory.path().join("auth.json");
        std::fs::write(&auth_path, "{}").unwrap();
        std::fs::write(
            directory.path().join("account.json"),
            r#"{"accountName":"Cached User","channelHandle":"@cached","accountPhotoUrl":"https://example.test/avatar"}"#,
        )
        .unwrap();

        let status = cached_auth_status(&auth_path);

        assert!(status.authenticated);
        assert_eq!(status.name, "Cached User");
        assert_eq!(status.handle, "@cached");
        assert_eq!(
            status.thumbnail.as_deref(),
            Some("https://example.test/avatar")
        );
    }

    #[test]
    fn missing_auth_file_is_logged_out_even_with_stale_account_metadata() {
        let directory = tempdir().unwrap();
        std::fs::write(directory.path().join("account.json"), r#"{"name":"Stale"}"#).unwrap();

        assert_eq!(
            cached_auth_status(&directory.path().join("auth.json")),
            AccountStatus::default()
        );
    }
}
