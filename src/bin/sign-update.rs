use std::{env, fs, path::PathBuf};

use anyhow::{Context as _, Result, anyhow, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use ed25519_dalek::{Signer as _, SigningKey};

const EXPECTED_PUBLIC_KEY_BASE64: &str = "NLyX3poppjaciLaPHu1ToiT4HFiwYIVdfBqN0r/yM4k=";

fn main() {
    if let Err(error) = run() {
        eprintln!("update manifest signing failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = env::args_os().skip(1);
    let manifest_path = PathBuf::from(args.next().context("manifest path is required")?);
    let signature_path = PathBuf::from(args.next().context("signature path is required")?);
    ensure!(args.next().is_none(), "unexpected extra arguments");

    let encoded_key =
        env::var("UPDATE_SIGNING_KEY_BASE64").context("UPDATE_SIGNING_KEY_BASE64 is required")?;
    let key = BASE64
        .decode(encoded_key.trim())
        .context("UPDATE_SIGNING_KEY_BASE64 is not valid base64")?;
    let key: [u8; 32] = key
        .try_into()
        .map_err(|_| anyhow!("UPDATE_SIGNING_KEY_BASE64 must contain exactly 32 bytes"))?;
    let signing_key = SigningKey::from_bytes(&key);
    let actual_public_key = BASE64.encode(signing_key.verifying_key().to_bytes());
    ensure!(
        actual_public_key == EXPECTED_PUBLIC_KEY_BASE64,
        "the signing key does not match the public key embedded in the app"
    );

    let manifest = fs::read(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let signature = signing_key.sign(&manifest);
    fs::write(&signature_path, signature.to_bytes())
        .with_context(|| format!("failed to write {}", signature_path.display()))?;
    Ok(())
}
