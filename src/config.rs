use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub python: String,
    pub auth_path: PathBuf,
    pub yt_dlp: String,
    pub ffmpeg: String,
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
            yt_dlp: std::env::var("POCKET_YTM_YTDLP").unwrap_or_else(|_| "yt-dlp".into()),
            ffmpeg: std::env::var("POCKET_YTM_FFMPEG").unwrap_or_else(|_| "ffmpeg".into()),
            cookies_path,
            language: std::env::var("POCKET_YTM_LANGUAGE").unwrap_or_else(|_| "ko".into()),
            location: std::env::var("POCKET_YTM_LOCATION").unwrap_or_else(|_| "KR".into()),
        }
    }
}

fn default_auth_path(manifest: &std::path::Path) -> PathBuf {
    let project_auth = manifest.join("auth.json");
    if project_auth.exists() {
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

fn path_env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}
