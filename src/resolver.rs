use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read as _, Write as _},
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

use anyhow::{Context as _, Result, anyhow, bail};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::AppConfig;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedMedia {
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub duration_seconds: Option<f64>,
}

pub struct MediaResolver {
    python: String,
    entrypoint: PathBuf,
    deno: Option<String>,
    cookies: Option<PathBuf>,
    process: Mutex<Option<ResolverSidecar>>,
    next_id: AtomicU64,
}

struct ResolverSidecar {
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

impl MediaResolver {
    pub fn new(config: &AppConfig) -> Arc<Self> {
        Arc::new(Self {
            python: config.python.clone(),
            entrypoint: config.media_resolver.clone(),
            deno: config.deno.clone(),
            cookies: config.cookies_path.clone(),
            process: Mutex::new(None),
            next_id: AtomicU64::new(1),
        })
    }

    pub fn warm_up(self: &Arc<Self>) {
        let resolver = self.clone();
        thread::Builder::new()
            .name("pocket-ytm-resolver-warmup".into())
            .spawn(move || {
                if let Err(error) = resolver.request("ping", json!({})) {
                    log::warn!("yt-dlp resolver warm-up failed: {error:#}");
                }
            })
            .expect("failed to start resolver warm-up thread");
    }

    pub fn resolve(&self, url: &str, format: &str) -> Result<ResolvedMedia> {
        let value = self.request("resolve", json!({"url": url, "format": format}))?;
        serde_json::from_value(value).context("yt-dlp resolver 응답 형식이 올바르지 않습니다")
    }

    fn request(&self, operation: &str, fields: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut request = fields;
        let object = request
            .as_object_mut()
            .context("resolver 요청 필드는 JSON 객체여야 합니다")?;
        object.insert("id".into(), id.into());
        object.insert("op".into(), operation.into());
        let mut guard = self.process.lock();

        for attempt in 0..2 {
            if guard.is_none() {
                *guard = Some(
                    self.spawn()
                        .context("yt-dlp resolver를 시작하지 못했습니다")?,
                );
            }
            let result = guard
                .as_mut()
                .expect("resolver initialized")
                .round_trip(id, &request);
            match result {
                Ok(envelope) => {
                    if !envelope.ok {
                        bail!(envelope.error);
                    }
                    return Ok(envelope.data);
                }
                Err(error) if attempt == 0 => {
                    log::warn!("restarting yt-dlp resolver after error: {error:#}");
                    *guard = None;
                }
                Err(error) => return Err(error),
            }
        }

        Err(anyhow!("yt-dlp resolver 요청에 실패했습니다"))
    }

    fn spawn(&self) -> Result<ResolverSidecar> {
        if !self.entrypoint.exists() {
            bail!("yt-dlp resolver not found: {}", self.entrypoint.display());
        }
        let is_python_script = self
            .entrypoint
            .extension()
            .is_some_and(|extension| extension == "py");
        let mut command = if is_python_script {
            let mut command = Command::new(&self.python);
            command.arg("-u").arg(&self.entrypoint);
            command
        } else {
            Command::new(&self.entrypoint)
        };
        if let Some(deno) = &self.deno {
            command.arg("--deno").arg(deno);
        }
        if let Some(cookies) = &self.cookies {
            command.arg("--cookies").arg(cookies);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().with_context(|| {
            format!(
                "yt-dlp resolver '{}'을(를) 시작할 수 없습니다",
                self.entrypoint.display()
            )
        })?;
        let stdin = child.stdin.take().context("resolver stdin unavailable")?;
        let stdout = BufReader::new(child.stdout.take().context("resolver stdout unavailable")?);
        Ok(ResolverSidecar {
            child,
            stdin,
            stdout,
        })
    }
}

impl ResolverSidecar {
    fn round_trip(&mut self, expected_id: u64, request: &Value) -> Result<Envelope> {
        serde_json::to_writer(&mut self.stdin, request)?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;

        let mut line = String::new();
        if self.stdout.read_line(&mut line)? == 0 {
            let status = self.child.try_wait()?.map(|status| status.to_string());
            let mut stderr = String::new();
            if let Some(mut pipe) = self.child.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            bail!(
                "yt-dlp resolver exited{}: {}",
                status
                    .map(|value| format!(" ({value})"))
                    .unwrap_or_default(),
                stderr.trim()
            );
        }
        let envelope: Envelope = serde_json::from_str(&line)
            .with_context(|| format!("invalid resolver response: {}", line.trim()))?;
        if envelope.id != expected_id {
            bail!(
                "resolver response id mismatch: expected {expected_id}, got {}",
                envelope.id
            );
        }
        Ok(envelope)
    }
}

impl Drop for ResolverSidecar {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
