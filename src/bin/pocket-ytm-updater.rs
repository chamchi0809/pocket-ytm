use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

use anyhow::{Context as _, Result, bail, ensure};

struct Arguments {
    parent_pid: u32,
    source: PathBuf,
    destination: PathBuf,
    log_root: PathBuf,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Pocket Music update failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let arguments = parse_arguments()?;
    validate_paths(&arguments)?;
    wait_for_parent(arguments.parent_pid);

    let destination_parent = arguments
        .destination
        .parent()
        .context("설치 대상의 상위 폴더가 없습니다")?;
    let backup = destination_parent.join(format!(
        ".Pocket Music.update-backup-{}.app",
        arguments.parent_pid
    ));
    if backup.exists() {
        fs::remove_dir_all(&backup)
            .with_context(|| format!("이전 백업을 지우지 못했습니다: {}", backup.display()))?;
    }

    fs::rename(&arguments.destination, &backup).with_context(|| {
        format!(
            "기존 앱을 백업하지 못했습니다: {} -> {}",
            arguments.destination.display(),
            backup.display()
        )
    })?;

    if let Err(error) = fs::rename(&arguments.source, &arguments.destination) {
        let rollback = fs::rename(&backup, &arguments.destination);
        return match rollback {
            Ok(()) => Err(error).context("새 앱을 설치하지 못해 기존 앱으로 복구했습니다"),
            Err(rollback_error) => bail!(
                "새 앱 설치 실패: {error}; 기존 앱 복구도 실패: {rollback_error}; 백업 위치: {}",
                backup.display()
            ),
        };
    }

    sync_parent(destination_parent)?;
    launch(&arguments.destination)?;

    if let Err(error) = fs::remove_dir_all(&backup) {
        eprintln!("기존 앱 백업을 지우지 못했습니다: {error}");
    }
    if let Err(error) = cleanup_staging(&arguments.log_root) {
        eprintln!("업데이트 임시 파일을 지우지 못했습니다: {error}");
    }
    Ok(())
}

fn parse_arguments() -> Result<Arguments> {
    let mut parent_pid = None;
    let mut source = None;
    let mut destination = None;
    let mut log_root = None;
    let mut args = env::args_os().skip(1);
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .with_context(|| format!("인자 값이 없습니다: {}", flag.to_string_lossy()))?;
        match flag.to_str() {
            Some("--parent-pid") => {
                parent_pid = Some(
                    value
                        .to_string_lossy()
                        .parse::<u32>()
                        .context("parent PID가 올바르지 않습니다")?,
                )
            }
            Some("--source") => source = Some(PathBuf::from(value)),
            Some("--destination") => destination = Some(PathBuf::from(value)),
            Some("--log-root") => log_root = Some(PathBuf::from(value)),
            _ => bail!("알 수 없는 인자입니다: {}", flag.to_string_lossy()),
        }
    }
    Ok(Arguments {
        parent_pid: parent_pid.context("--parent-pid가 필요합니다")?,
        source: source.context("--source가 필요합니다")?,
        destination: destination.context("--destination이 필요합니다")?,
        log_root: log_root.context("--log-root가 필요합니다")?,
    })
}

fn validate_paths(arguments: &Arguments) -> Result<()> {
    ensure!(
        arguments.source.is_absolute(),
        "업데이트 원본은 절대 경로여야 합니다"
    );
    ensure!(
        arguments.destination.is_absolute(),
        "설치 대상은 절대 경로여야 합니다"
    );
    ensure!(
        arguments.log_root.is_absolute(),
        "로그 폴더는 절대 경로여야 합니다"
    );
    ensure!(arguments.source.is_dir(), "업데이트 앱 번들이 없습니다");
    ensure!(arguments.destination.is_dir(), "기존 앱 번들이 없습니다");
    ensure!(
        arguments
            .source
            .extension()
            .and_then(|value| value.to_str())
            == Some("app")
            && arguments
                .destination
                .extension()
                .and_then(|value| value.to_str())
                == Some("app"),
        "업데이트 대상은 .app 번들이어야 합니다"
    );
    let source = arguments.source.canonicalize()?;
    let log_root = arguments.log_root.canonicalize()?;
    ensure!(
        source.starts_with(&log_root),
        "업데이트 원본이 승인된 임시 폴더 밖에 있습니다"
    );
    ensure!(
        arguments.source != arguments.destination,
        "업데이트 원본과 설치 대상이 같습니다"
    );
    Ok(())
}

fn wait_for_parent(parent_pid: u32) {
    for _ in 0..120 {
        if !process_is_running(parent_pid) {
            return;
        }
        thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(unix)]
fn process_is_running(pid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(windows)]
fn process_is_running(pid: u32) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
}

#[cfg(target_os = "macos")]
fn launch(bundle: &Path) -> Result<()> {
    Command::new("/usr/bin/open")
        .arg(bundle)
        .spawn()
        .context("업데이트된 앱을 다시 실행하지 못했습니다")?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn launch(_bundle: &Path) -> Result<()> {
    bail!("이 updater helper는 현재 macOS 앱 번들만 지원합니다")
}

fn sync_parent(parent: &Path) -> Result<()> {
    let directory = fs::File::open(parent)
        .with_context(|| format!("설치 폴더를 열지 못했습니다: {}", parent.display()))?;
    directory
        .sync_all()
        .context("설치 폴더 변경 사항을 디스크에 반영하지 못했습니다")
}

fn cleanup_staging(log_root: &Path) -> Result<()> {
    let parent = log_root
        .parent()
        .context("업데이트 임시 폴더의 상위 경로가 없습니다")?;
    ensure!(
        parent.file_name().and_then(|value| value.to_str()) == Some("updates"),
        "예상하지 못한 업데이트 임시 경로입니다"
    );
    fs::remove_dir_all(log_root).with_context(|| {
        format!(
            "업데이트 임시 폴더를 지우지 못했습니다: {}",
            log_root.display()
        )
    })
}
