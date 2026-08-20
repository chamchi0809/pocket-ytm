use std::{
    io::{BufRead as _, BufReader, Write as _},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::mpsc,
    thread,
};

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

const E2E_ADDRESS_ENV: &str = "POCKET_YTM_E2E_ADDR";

#[derive(Debug, Deserialize)]
#[serde(tag = "command", rename_all = "camelCase")]
pub enum E2eCommand {
    GetState,
    Search { query: String },
    OpenSectionItem { section: usize, index: usize },
    PlaySectionItem { section: usize, index: usize },
    OpenSearchResult { index: usize },
    PlaySearchResult { index: usize },
    OpenDetailItem { section: usize, index: usize },
    PlayDetailItem { section: usize, index: usize },
    Seek { seconds: f64 },
    TogglePlayback,
    NextTrack,
    PreviousTrack,
    Quit,
}

pub struct E2eRequest {
    pub command: E2eCommand,
    pub response: mpsc::Sender<Value>,
}

pub struct E2eHarness {
    requests: mpsc::Receiver<E2eRequest>,
}

impl E2eHarness {
    pub fn from_env() -> Option<Self> {
        let address = std::env::var(E2E_ADDRESS_ENV).ok()?;
        match Self::bind(&address) {
            Ok(harness) => Some(harness),
            Err(error) => {
                log::error!("E2E control channel unavailable: {error:#}");
                None
            }
        }
    }

    pub fn drain(&self) -> Vec<E2eRequest> {
        self.requests.try_iter().collect()
    }

    fn bind(address: &str) -> Result<Self> {
        let address: SocketAddr = address
            .parse()
            .with_context(|| format!("invalid {E2E_ADDRESS_ENV}: {address}"))?;
        if !address.ip().is_loopback() {
            bail!("{E2E_ADDRESS_ENV} must use a loopback address");
        }
        let listener = TcpListener::bind(address)
            .with_context(|| format!("failed to bind E2E control channel at {address}"))?;
        let (requests, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("pocket-ytm-e2e".into())
            .spawn(move || accept_connections(listener, requests))
            .context("failed to start E2E control channel")?;
        log::info!("E2E control channel listening at {address}");
        Ok(Self { requests: receiver })
    }
}

fn accept_connections(listener: TcpListener, requests: mpsc::Sender<E2eRequest>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        if let Err(error) = handle_connection(stream, &requests) {
            log::debug!("E2E request failed: {error:#}");
        }
    }
}

fn handle_connection(mut stream: TcpStream, requests: &mpsc::Sender<E2eRequest>) -> Result<()> {
    let peer = stream.peer_addr().context("missing E2E peer address")?;
    if !peer.ip().is_loopback() {
        bail!("rejected non-loopback E2E peer: {peer}");
    }
    let mut request = String::new();
    BufReader::new(stream.try_clone().context("failed to clone E2E stream")?)
        .read_line(&mut request)
        .context("failed to read E2E request")?;
    let command: E2eCommand = serde_json::from_str(request.trim())
        .context("E2E request is not a supported JSON command")?;
    let (response, receiver) = mpsc::channel();
    requests
        .send(E2eRequest { command, response })
        .context("app E2E receiver closed")?;
    let response = receiver.recv().context("app did not answer E2E request")?;
    serde_json::to_writer(&mut stream, &response).context("failed to write E2E response")?;
    stream
        .write_all(b"\n")
        .context("failed to finish E2E response")?;
    Ok(())
}

pub fn ok(data: Value) -> Value {
    json!({"ok": true, "data": data})
}

pub fn error(message: impl Into<String>) -> Value {
    json!({"ok": false, "error": message.into()})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_use_a_small_stable_json_protocol() {
        assert!(matches!(
            serde_json::from_str::<E2eCommand>(r#"{"command":"getState"}"#).unwrap(),
            E2eCommand::GetState
        ));
        assert!(matches!(
            serde_json::from_str::<E2eCommand>(r#"{"command":"playSearchResult","index":3}"#)
                .unwrap(),
            E2eCommand::PlaySearchResult { index: 3 }
        ));
        assert!(matches!(
            serde_json::from_str::<E2eCommand>(
                r#"{"command":"openDetailItem","section":2,"index":1}"#
            )
            .unwrap(),
            E2eCommand::OpenDetailItem {
                section: 2,
                index: 1
            }
        ));
        assert!(matches!(
            serde_json::from_str::<E2eCommand>(r#"{"command":"nextTrack"}"#).unwrap(),
            E2eCommand::NextTrack
        ));
        assert!(matches!(
            serde_json::from_str::<E2eCommand>(
                r#"{"command":"playSectionItem","section":1,"index":4}"#
            )
            .unwrap(),
            E2eCommand::PlaySectionItem {
                section: 1,
                index: 4
            }
        ));
    }

    #[test]
    fn public_bind_addresses_are_rejected() {
        let error = E2eHarness::bind("0.0.0.0:0")
            .err()
            .expect("public address should be rejected");
        assert!(error.to_string().contains("loopback"));
    }

    #[test]
    fn loopback_detection_accepts_ipv4_and_ipv6() {
        assert!(std::net::IpAddr::from([127, 0, 0, 1]).is_loopback());
        assert!(std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST).is_loopback());
    }
}
