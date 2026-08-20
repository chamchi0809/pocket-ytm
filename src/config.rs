use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub python: String,
    pub auth_path: PathBuf,
    pub settings_path: PathBuf,
    pub media_resolver: PathBuf,
    pub ffmpeg: String,
    pub deno: Option<String>,
    pub pot_provider: Option<PathBuf>,
    pub cookies_path: Option<PathBuf>,
    pub language: String,
    pub location: String,
}

impl AppConfig {
    pub fn from_env() -> Self {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let venv_python = if cfg!(target_os = "windows") {
            manifest.join(".venv").join("Scripts").join("python.exe")
        } else {
            manifest.join(".venv").join("bin").join("python")
        };

        let python = std::env::var("POCKET_YTM_PYTHON").unwrap_or_else(|_| {
            if venv_python.exists() {
                venv_python.to_string_lossy().into_owned()
            } else {
                "python3".into()
            }
        });

        let auth_path = path_env("POCKET_YTM_AUTH").unwrap_or_else(|| default_auth_path(&manifest));
        let settings_path = path_env("POCKET_YTM_SETTINGS")
            .unwrap_or_else(|| auth_path.with_file_name("settings.json"));
        let project_cookies = manifest.join("cookies.txt");
        let cookies_path = path_env("POCKET_YTM_COOKIES").or_else(|| {
            if project_cookies.exists() {
                Some(project_cookies)
            } else {
                Some(auth_path.with_file_name("cookies.txt"))
            }
        });

        Self {
            python,
            auth_path,
            settings_path,
            media_resolver: path_env("POCKET_YTM_RESOLVER")
                .or_else(|| bundled_tool_path("pocket-ytm-resolver"))
                .unwrap_or_else(|| manifest.join("backend/yt_dlp_resolver.py")),
            ffmpeg: tool_from_env_or_bundle("POCKET_YTM_FFMPEG", "ffmpeg", "ffmpeg"),
            deno: std::env::var("POCKET_YTM_DENO")
                .ok()
                .filter(|value| !value.is_empty())
                .or_else(|| {
                    bundled_tool_path("deno").map(|path| path.to_string_lossy().into_owned())
                }),
            pot_provider: path_env("POCKET_YTM_POT_PROVIDER")
                .filter(|path| path.is_dir())
                .or_else(|| bundled_resource_dir("bgutil-ytdlp-pot-provider/server"))
                .or_else(|| {
                    let path = manifest
                        .join("target/macos-dependencies/share/bgutil-ytdlp-pot-provider/server");
                    path.is_dir().then_some(path)
                }),
            cookies_path,
            language: std::env::var("POCKET_YTM_LANGUAGE").unwrap_or_else(|_| "ko".into()),
            location: std::env::var("POCKET_YTM_LOCATION").unwrap_or_else(|_| "KR".into()),
        }
    }
}

fn tool_from_env_or_bundle(variable: &str, bundled_name: &str, fallback: &str) -> String {
    std::env::var(variable)
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| bundled_tool_path(bundled_name).map(|path| path.to_string_lossy().into_owned()))
        .unwrap_or_else(|| fallback.to_owned())
}

pub(crate) fn bundled_tool_path(name: &str) -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    bundled_tool_path_for_executable(&executable, name).filter(|path| path.is_file())
}

fn bundled_resource_dir(name: &str) -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    bundled_resource_dir_for_executable(&executable, name).filter(|path| path.is_dir())
}

fn bundled_resource_dir_for_executable(
    executable: &std::path::Path,
    name: &str,
) -> Option<PathBuf> {
    let macos = executable.parent()?;
    let contents = macos.parent()?;
    if macos.file_name()? != "MacOS" || contents.file_name()? != "Contents" {
        return None;
    }
    Some(contents.join("Resources/share").join(name))
}

fn bundled_tool_path_for_executable(executable: &std::path::Path, name: &str) -> Option<PathBuf> {
    let macos = executable.parent()?;
    let contents = macos.parent()?;
    if macos.file_name()? != "MacOS" || contents.file_name()? != "Contents" {
        return None;
    }
    let resources = contents.join("Resources");
    if matches!(name, "pocket-ytm-bridge" | "pocket-ytm-resolver") {
        Some(resources.join("libexec").join(name).join(name))
    } else {
        Some(resources.join("bin").join(name))
    }
}

fn default_auth_path(manifest: &std::path::Path) -> PathBuf {
    let project_auth = manifest.join("auth.json");
    let is_packaged_app = std::env::current_exe()
        .ok()
        .is_some_and(|executable| is_app_bundle_executable(&executable));
    if !is_packaged_app && project_auth.exists() {
        return project_auth;
    }

    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join("Library/Application Support/Pocket Music/auth.json");
    }

    #[cfg(target_os = "windows")]
    if let Some(app_data) = std::env::var_os("APPDATA") {
        return PathBuf::from(app_data).join("Pocket Music/auth.json");
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME") {
            return PathBuf::from(config_home).join("pocket-music/auth.json");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".config/pocket-music/auth.json");
        }
    }

    project_auth
}

fn is_app_bundle_executable(executable: &std::path::Path) -> bool {
    executable
        .parent()
        .zip(executable.parent().and_then(std::path::Path::parent))
        .is_some_and(|(macos, contents)| {
            macos.file_name().is_some_and(|name| name == "MacOS")
                && contents.file_name().is_some_and(|name| name == "Contents")
        })
}

fn path_env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_tools_are_resolved_relative_to_the_app_executable() {
        let executable = PathBuf::from("/Applications/Pocket Music.app/Contents/MacOS/pocket-ytm");

        assert_eq!(
            bundled_tool_path_for_executable(&executable, "ffmpeg"),
            Some(PathBuf::from(
                "/Applications/Pocket Music.app/Contents/Resources/bin/ffmpeg"
            ))
        );
        assert_eq!(
            bundled_resource_dir_for_executable(&executable, "bgutil-ytdlp-pot-provider/server"),
            Some(PathBuf::from(
                "/Applications/Pocket Music.app/Contents/Resources/share/bgutil-ytdlp-pot-provider/server"
            ))
        );
        assert_eq!(
            bundled_tool_path_for_executable(&executable, "pocket-ytm-bridge"),
            Some(PathBuf::from(
                "/Applications/Pocket Music.app/Contents/Resources/libexec/pocket-ytm-bridge/pocket-ytm-bridge"
            ))
        );
    }

    #[test]
    fn development_binaries_do_not_look_like_app_bundles() {
        let executable = PathBuf::from("/project/target/debug/pocket-ytm");

        assert_eq!(
            bundled_tool_path_for_executable(&executable, "ffmpeg"),
            None
        );
    }

    #[test]
    fn packaged_executables_are_detected_without_reading_the_source_tree() {
        assert!(is_app_bundle_executable(std::path::Path::new(
            "/Applications/Pocket Music.app/Contents/MacOS/pocket-ytm"
        )));
        assert!(!is_app_bundle_executable(std::path::Path::new(
            "/project/target/release/pocket-ytm"
        )));
    }
}
