use std::{
    io::{BufRead as _, BufReader, Write as _},
    net::TcpStream,
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use serde_json::Value;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let address = args
        .next()
        .context("usage: pocket-ytm-e2e <loopback-address> '<json-command>'")?;
    let command = args
        .next()
        .context("usage: pocket-ytm-e2e <loopback-address> '<json-command>'")?;
    if args.next().is_some() {
        bail!("too many arguments");
    }
    let _: Value = serde_json::from_str(&command).context("command must be valid JSON")?;
    let mut stream = TcpStream::connect(&address)
        .with_context(|| format!("failed to connect to Pocket Music at {address}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .context("failed to configure E2E timeout")?;
    stream
        .write_all(command.as_bytes())
        .context("failed to send E2E command")?;
    stream
        .write_all(b"\n")
        .context("failed to finish E2E command")?;
    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .context("failed to read E2E response")?;
    let response: Value =
        serde_json::from_str(response.trim()).context("app returned invalid E2E JSON")?;
    println!("{}", serde_json::to_string(&response)?);
    if response.get("ok") == Some(&Value::Bool(false)) {
        std::process::exit(1);
    }
    Ok(())
}
