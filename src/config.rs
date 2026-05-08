// src/config.rs – Persist / load settings and read the cookie file.
use std::path::PathBuf;
use anyhow::Context;
use serde::{Deserialize, Serialize};

// ── Directory helpers ─────────────────────────────────────────────────────

/// Returns the udemyget config directory, creating it if it doesn't exist.
///
/// Platform paths (from the `dirs` crate):
///   Linux   : $XDG_CONFIG_HOME/udemyget  (~/.config/udemyget)
///   macOS   : ~/Library/Application Support/udemyget
///   Windows : %APPDATA%\udemyget
fn config_dir() -> PathBuf {
    let dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("udemyget");

    // Create eagerly — every caller can assume the directory exists.
    if !dir.exists() {
        let _ = std::fs::create_dir_all(&dir);
    }

    dir
}

fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

/// Absolute path to the cookie file.
pub fn cookie_file_path() -> PathBuf {
    config_dir().join("cookies.txt")
}

/// Short, human-readable path for display in the TUI.
///
/// Replaces the home-directory prefix with `~` and uses the platform's
/// native path separator, so the string stays short on all OSes:
///
///   Linux   : ~/.config/udemyget/cookies.txt               (30 chars)
///   macOS   : ~/Library/Application Support/udemyget/cookies.txt (50 chars)
///   Windows : ~\AppData\Roaming\udemyget\cookies.txt        (38 chars)
pub fn cookie_file_display() -> String {
    let path = cookie_file_path();
    if let Some(home) = dirs::home_dir() {
        if let Ok(rel) = path.strip_prefix(&home) {
            // std::path::MAIN_SEPARATOR is '/' on Unix, '\\' on Windows.
            return format!("~{}{}", std::path::MAIN_SEPARATOR, rel.display());
        }
    }
    path.display().to_string()
}

// ── Cookie file I/O ───────────────────────────────────────────────────────

/// Read and return the cookie string from `cookies.txt`.
/// Returns `Err` if the file is missing or empty.
pub fn read_cookie_file() -> anyhow::Result<String> {
    let path = cookie_file_path();
    if !path.exists() {
        anyhow::bail!("file not found: {}", cookie_file_display());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", cookie_file_display()))?;
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        anyhow::bail!("file is empty");
    }
    Ok(trimmed)
}

/// Create `cookies.txt` with a step-by-step placeholder comment on first run.
/// The directory is guaranteed to exist because `config_dir()` creates it.
pub fn ensure_cookie_file() {
    let path = cookie_file_path();
    if path.exists() {
        return;
    }
    let placeholder = "\
# Paste your Udemy Cookie header value on the line below and save.
# How to get it:
#   1. Log in to udemy.com in Firefox / Chrome
#   2. DevTools (F12) -> Network tab -> reload page
#   3. Click any request to udemy.com
#   4. Request Headers -> right-click Cookie -> Copy value
#   5. Replace this entire comment with that value and save.
";
    let _ = std::fs::write(&path, placeholder);
}

// ── App config (output dir, etc.) ─────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Runtime-only cookie string loaded from cookies.txt (never written to disk).
    #[serde(skip)]
    pub cookie: String,
    /// Where to put downloaded files.
    pub output_dir: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cookie: String::new(),
            output_dir: default_output_dir(),
        }
    }
}

fn default_output_dir() -> String {
    dirs::download_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")))
        .join("Udemy")
        .to_string_lossy()
        .into_owned()
}

pub fn load() -> Config {
    let path = config_path();
    if !path.exists() {
        return Config::default();
    }
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    serde_json::from_str::<Config>(&text).unwrap_or_default()
}

pub fn save(config: &Config) -> anyhow::Result<()> {
    let path = config_path();
    // config_dir() already creates the directory; this is a belt-and-braces
    // guard in case save() is ever called before App::new().
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create config dir {:?}", parent))?;
    }
    let text = serde_json::to_string_pretty(config)?;
    std::fs::write(&path, text)
        .with_context(|| format!("write config {:?}", path))?;
    Ok(())
}

// ── Output path builder ───────────────────────────────────────────────────

/// `{output_dir}/{course}/{chap_idx:02}. {chapter}/{lec_idx:03}. {lecture}.mp4`
pub fn lecture_output_path(
    output_dir: &str,
    course_title: &str,
    chapter_index: u32,
    chapter_title: &str,
    lecture_index: u32,
    lecture_title: &str,
) -> String {
    let course_dir = sanitize_filename::sanitize(course_title);
    let chap_dir = format!(
        "{:02}. {}",
        chapter_index,
        sanitize_filename::sanitize(chapter_title)
    );
    let filename = format!(
        "{:03}. {}.mp4",
        lecture_index,
        sanitize_filename::sanitize(lecture_title)
    );
    std::path::Path::new(output_dir)
        .join(course_dir)
        .join(chap_dir)
        .join(filename)
        .to_string_lossy()
        .into_owned()
}
