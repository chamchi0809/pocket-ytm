use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use ed25519_dalek::{Signature, VerifyingKey};
use reqwest::blocking::Client;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const REPOSITORY: &str = "chamchi0809/pocket-ytm";
const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/chamchi0809/pocket-ytm/releases/latest";
const MANIFEST_ASSET: &str = "update-manifest.json";
const SIGNATURE_ASSET: &str = "update-manifest.json.sig";
const UPDATE_PUBLIC_KEY_BASE64: &str = "NLyX3poppjaciLaPHu1ToiT4HFiwYIVdfBqN0r/yM4k=";
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct AvailableUpdate {
    pub version: String,
    pub notes: String,
    pub release_url: String,
    asset_url: String,
    sha256: String,
    size: u64,
}

#[derive(Debug, Clone)]
pub enum UpdateCheck {
    UpToDate,
    Available(AvailableUpdate),
}

#[derive(Debug, Clone)]
pub struct UpdateClient {
    client: Client,
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    body: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateManifest {
    schema_version: u32,
    version: String,
    repository: String,
    platforms: std::collections::HashMap<String, PlatformAsset>,
}

#[derive(Debug, Deserialize)]
struct PlatformAsset {
    url: String,
    sha256: String,
    size: u64,
}

impl UpdateClient {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .user_agent(format!(
                    "Pocket-Music/{} (+https://github.com/{REPOSITORY})",
                    env!("CARGO_PKG_VERSION")
                ))
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(45))
                .build()
                .context("업데이트 HTTP 클라이언트를 만들지 못했습니다")?,
        })
    }

    pub fn check(&self) -> Result<UpdateCheck> {
        let release: GitHubRelease = self
            .client
            .get(LATEST_RELEASE_API)
            .send()
            .context("GitHub Releases에 연결하지 못했습니다")?
            .error_for_status()
            .context("최신 GitHub 릴리스를 가져오지 못했습니다")?
            .json()
            .context("GitHub 릴리스 응답을 읽지 못했습니다")?;

        let manifest_url = release_asset_url(&release, MANIFEST_ASSET)?;
        let signature_url = release_asset_url(&release, SIGNATURE_ASSET)?;
        let manifest_bytes = self.download_small(&manifest_url, MAX_MANIFEST_BYTES)?;
        let signature_bytes = self.download_small(&signature_url, 128)?;
        let manifest = verify_manifest(&manifest_bytes, &signature_bytes)?;

        ensure!(
            manifest.schema_version == 1,
            "지원하지 않는 업데이트 manifest 버전입니다: {}",
            manifest.schema_version
        );
        ensure!(
            manifest.repository == REPOSITORY,
            "업데이트 manifest의 저장소가 일치하지 않습니다"
        );

        let release_version = parse_version(&release.tag_name)?;
        let manifest_version = parse_version(&manifest.version)?;
        ensure!(
            release_version == manifest_version,
            "릴리스 태그와 manifest 버전이 일치하지 않습니다"
        );

        let current_version = Version::parse(env!("CARGO_PKG_VERSION"))
            .context("현재 앱 버전이 올바른 semver가 아닙니다")?;
        if manifest_version <= current_version {
            return Ok(UpdateCheck::UpToDate);
        }

        let platform = current_platform()?;
        let asset = manifest
            .platforms
            .get(platform)
            .ok_or_else(|| anyhow!("이 시스템용 업데이트 파일이 없습니다: {platform}"))?;
        validate_sha256(&asset.sha256)?;
        ensure!(
            asset.size > 0 && asset.size <= MAX_ARCHIVE_BYTES,
            "업데이트 파일 크기가 허용 범위를 벗어났습니다"
        );

        Ok(UpdateCheck::Available(AvailableUpdate {
            version: manifest.version,
            notes: release.body,
            release_url: release.html_url,
            asset_url: asset.url.clone(),
            sha256: asset.sha256.to_ascii_lowercase(),
            size: asset.size,
        }))
    }

    pub fn download_and_prepare_install(&self, update: &AvailableUpdate) -> Result<()> {
        #[cfg(not(target_os = "macos"))]
        {
            let _ = update;
            bail!("현재 자동 교체 설치는 macOS 번들에서만 지원합니다");
        }

        #[cfg(target_os = "macos")]
        self.download_and_prepare_macos_install(update)
    }

    fn download_small(&self, url: &str, limit: usize) -> Result<Vec<u8>> {
        ensure_github_download_url(url)?;
        let bytes = self
            .client
            .get(url)
            .send()
            .with_context(|| format!("업데이트 메타데이터를 내려받지 못했습니다: {url}"))?
            .error_for_status()
            .context("업데이트 메타데이터 다운로드가 실패했습니다")?
            .bytes()
            .context("업데이트 메타데이터를 읽지 못했습니다")?;
        ensure!(bytes.len() <= limit, "업데이트 메타데이터가 너무 큽니다");
        Ok(bytes.to_vec())
    }

    #[cfg(target_os = "macos")]
    fn download_and_prepare_macos_install(&self, update: &AvailableUpdate) -> Result<()> {
        let current_bundle = current_macos_bundle()?;
        let helper = current_bundle.join("Contents/MacOS/pocket-ytm-updater");
        ensure!(helper.is_file(), "앱 번들에 업데이트 helper가 없습니다");

        let update_root = update_root()?.join(format!("{}-{}", update.version, std::process::id()));
        if update_root.exists() {
            fs::remove_dir_all(&update_root).with_context(|| {
                format!(
                    "이전 업데이트 임시 폴더를 지우지 못했습니다: {}",
                    update_root.display()
                )
            })?;
        }
        fs::create_dir_all(&update_root).with_context(|| {
            format!(
                "업데이트 임시 폴더를 만들지 못했습니다: {}",
                update_root.display()
            )
        })?;

        let archive_path = update_root.join("Pocket-Music.zip");
        self.download_archive(update, &archive_path)?;
        let extracted = update_root.join("extracted");
        fs::create_dir_all(&extracted)?;
        extract_zip(&archive_path, &extracted)?;
        let staged_bundle = find_staged_bundle(&extracted)?;
        validate_staged_bundle(&staged_bundle, &update.version)?;

        let log_path = update_root.join("updater.log");
        let stdout = File::create(&log_path).with_context(|| {
            format!("업데이트 로그를 만들지 못했습니다: {}", log_path.display())
        })?;
        let stderr = stdout
            .try_clone()
            .context("업데이트 로그 파일을 복제하지 못했습니다")?;

        Command::new(&helper)
            .arg("--parent-pid")
            .arg(std::process::id().to_string())
            .arg("--source")
            .arg(&staged_bundle)
            .arg("--destination")
            .arg(&current_bundle)
            .arg("--log-root")
            .arg(&update_root)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .context("업데이트 helper를 실행하지 못했습니다")?;

        Ok(())
    }

    fn download_archive(&self, update: &AvailableUpdate, destination: &Path) -> Result<()> {
        ensure_github_download_url(&update.asset_url)?;
        let mut response = self
            .client
            .get(&update.asset_url)
            .send()
            .context("업데이트 파일을 내려받지 못했습니다")?
            .error_for_status()
            .context("업데이트 파일 다운로드가 실패했습니다")?;

        if let Some(length) = response.content_length() {
            ensure!(
                length == update.size,
                "업데이트 파일 크기가 manifest와 다릅니다"
            );
        }

        let mut output = File::create(destination).with_context(|| {
            format!(
                "업데이트 파일을 만들지 못했습니다: {}",
                destination.display()
            )
        })?;
        let mut hasher = Sha256::new();
        let mut total = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = response
                .read(&mut buffer)
                .context("업데이트 파일을 읽지 못했습니다")?;
            if read == 0 {
                break;
            }
            total += read as u64;
            ensure!(total <= MAX_ARCHIVE_BYTES, "업데이트 파일이 너무 큽니다");
            hasher.update(&buffer[..read]);
            output
                .write_all(&buffer[..read])
                .context("업데이트 파일을 저장하지 못했습니다")?;
        }
        output
            .sync_all()
            .context("업데이트 파일을 디스크에 반영하지 못했습니다")?;

        ensure!(
            total == update.size,
            "업데이트 파일 크기가 manifest와 다릅니다"
        );
        let digest = format!("{:x}", hasher.finalize());
        ensure!(
            digest == update.sha256,
            "업데이트 파일의 SHA-256이 일치하지 않습니다"
        );
        Ok(())
    }
}

fn release_asset_url(release: &GitHubRelease, name: &str) -> Result<String> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .map(|asset| asset.browser_download_url.clone())
        .ok_or_else(|| anyhow!("릴리스에 {name} 파일이 없습니다"))
}

fn verify_manifest(bytes: &[u8], signature: &[u8]) -> Result<UpdateManifest> {
    let public_key = BASE64
        .decode(UPDATE_PUBLIC_KEY_BASE64)
        .context("내장 업데이트 공개키를 읽지 못했습니다")?;
    let public_key: [u8; 32] = public_key
        .try_into()
        .map_err(|_| anyhow!("내장 업데이트 공개키 길이가 잘못되었습니다"))?;
    let verifying_key =
        VerifyingKey::from_bytes(&public_key).context("내장 업데이트 공개키가 잘못되었습니다")?;
    let signature =
        Signature::from_slice(signature).context("업데이트 manifest 서명 형식이 잘못되었습니다")?;
    verifying_key
        .verify_strict(bytes, &signature)
        .context("업데이트 manifest 서명이 유효하지 않습니다")?;
    serde_json::from_slice(bytes).context("업데이트 manifest JSON을 읽지 못했습니다")
}

fn parse_version(value: &str) -> Result<Version> {
    Version::parse(value.trim_start_matches('v'))
        .with_context(|| format!("올바르지 않은 릴리스 버전입니다: {value}"))
}

fn current_platform() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("macos-aarch64"),
        (os, arch) => bail!("자동 업데이트가 아직 지원되지 않는 시스템입니다: {os}-{arch}"),
    }
}

fn validate_sha256(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "manifest의 SHA-256이 잘못되었습니다"
    );
    Ok(())
}

fn ensure_github_download_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).context("업데이트 URL이 올바르지 않습니다")?;
    ensure!(
        parsed.scheme() == "https" && parsed.host_str() == Some("github.com"),
        "업데이트 파일은 github.com HTTPS 주소에서만 받을 수 있습니다"
    );
    ensure!(
        parsed
            .path()
            .starts_with(&format!("/{REPOSITORY}/releases/download/")),
        "업데이트 파일 URL이 공식 릴리스 경로가 아닙니다"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn current_macos_bundle() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("현재 실행 파일 경로를 찾지 못했습니다")?;
    let macos_dir = executable
        .parent()
        .ok_or_else(|| anyhow!("현재 실행 파일의 상위 폴더가 없습니다"))?;
    let contents_dir = macos_dir
        .parent()
        .ok_or_else(|| anyhow!("앱 Contents 폴더를 찾지 못했습니다"))?;
    let bundle = contents_dir
        .parent()
        .ok_or_else(|| anyhow!("앱 번들을 찾지 못했습니다"))?;
    ensure!(
        macos_dir.file_name().and_then(|name| name.to_str()) == Some("MacOS")
            && contents_dir.file_name().and_then(|name| name.to_str()) == Some("Contents")
            && bundle.extension().and_then(|extension| extension.to_str()) == Some("app"),
        "자동 업데이트는 패키징된 .app에서만 실행할 수 있습니다"
    );
    Ok(bundle.to_path_buf())
}

#[cfg(target_os = "macos")]
fn update_root() -> Result<PathBuf> {
    let home =
        std::env::var_os("HOME").ok_or_else(|| anyhow!("사용자 홈 폴더를 찾지 못했습니다"))?;
    Ok(PathBuf::from(home).join("Library/Caches/Pocket Music/updates"))
}

#[cfg(target_os = "macos")]
fn extract_zip(archive_path: &Path, destination: &Path) -> Result<()> {
    let archive_file = File::open(archive_path).with_context(|| {
        format!(
            "업데이트 압축 파일을 열지 못했습니다: {}",
            archive_path.display()
        )
    })?;
    let mut archive =
        zip::ZipArchive::new(archive_file).context("업데이트 ZIP을 읽지 못했습니다")?;

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .context("업데이트 ZIP 항목을 읽지 못했습니다")?;
        let relative = entry
            .enclosed_name()
            .ok_or_else(|| anyhow!("업데이트 ZIP에 안전하지 않은 경로가 있습니다"))?;
        let output_path = destination.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&output_path)?;
            continue;
        }
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = File::create(&output_path)?;
        io::copy(&mut entry, &mut output)?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&output_path, fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn find_staged_bundle(extracted: &Path) -> Result<PathBuf> {
    let direct = extracted.join("Pocket Music.app");
    if direct.is_dir() {
        return Ok(direct);
    }
    let bundles: Vec<_> = fs::read_dir(extracted)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("app"))
        .collect();
    match bundles.as_slice() {
        [bundle] if bundle.is_dir() => Ok(bundle.clone()),
        _ => bail!("업데이트 ZIP에서 Pocket Music.app을 찾지 못했습니다"),
    }
}

#[cfg(target_os = "macos")]
fn validate_staged_bundle(bundle: &Path, expected_version: &str) -> Result<()> {
    let executable = bundle.join("Contents/MacOS/pocket-ytm");
    let helper = bundle.join("Contents/MacOS/pocket-ytm-updater");
    let info = bundle.join("Contents/Info.plist");
    ensure!(executable.is_file(), "업데이트 앱 실행 파일이 없습니다");
    ensure!(helper.is_file(), "업데이트 앱 helper가 없습니다");
    ensure!(info.is_file(), "업데이트 앱 Info.plist가 없습니다");

    let output = Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", "Print :CFBundleIdentifier"])
        .arg(&info)
        .output()
        .context("업데이트 앱의 bundle ID를 확인하지 못했습니다")?;
    ensure!(
        output.status.success(),
        "업데이트 앱의 bundle ID를 읽지 못했습니다"
    );
    ensure!(
        String::from_utf8_lossy(&output.stdout).trim() == "dev.pocket.ytm",
        "업데이트 앱의 bundle ID가 일치하지 않습니다"
    );

    let output = Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", "Print :CFBundleShortVersionString"])
        .arg(&info)
        .output()
        .context("업데이트 앱 버전을 확인하지 못했습니다")?;
    ensure!(
        output.status.success(),
        "업데이트 앱 버전을 읽지 못했습니다"
    );
    ensure!(
        String::from_utf8_lossy(&output.stdout).trim() == expected_version,
        "업데이트 앱 버전이 manifest와 일치하지 않습니다"
    );

    let status = Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(bundle)
        .status()
        .context("업데이트 앱의 코드 서명을 확인하지 못했습니다")?;
    ensure!(
        status.success(),
        "업데이트 앱의 코드 서명이 유효하지 않습니다"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_official_release_downloads_are_accepted() {
        assert!(
            ensure_github_download_url(
            "https://github.com/chamchi0809/pocket-ytm/releases/download/v0.2.0/Pocket-Music.zip"
            )
            .is_ok()
        );
        assert!(ensure_github_download_url("https://example.com/Pocket-Music.zip").is_err());
        assert!(
            ensure_github_download_url(
                "https://github.com/other/project/releases/download/v0.2.0/Pocket-Music.zip"
            )
            .is_err()
        );
    }

    #[test]
    fn versions_allow_a_leading_v() {
        assert_eq!(parse_version("v1.2.3").unwrap(), Version::new(1, 2, 3));
        assert!(parse_version("latest").is_err());
    }

    #[test]
    fn sha256_must_be_complete_hex() {
        assert!(validate_sha256(&"a".repeat(64)).is_ok());
        assert!(validate_sha256(&"z".repeat(64)).is_err());
        assert!(validate_sha256(&"a".repeat(63)).is_err());
    }
}
