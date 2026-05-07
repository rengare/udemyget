// src/config.rs – Persist / load settings and read the cookie file.
use std::path::PathBuf;
use anyhow::Context;
use serde::{Deserialize, Serialize};

// ── Directory helpers ─────────────────────────────────────────────────────

fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("udemyget")
}

fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

/// Path where the user must paste their Udemy Cookie header string.
/// Displayed prominently in the UI so the user knows exactly where to edit.
pub fn cookie_file_path() -> PathBuf {
    config_dir().join("cookies.txt")
}

// ── Cookie file I/O ───────────────────────────────────────────────────────

/// Read and return the cookie string from `cookies.txt`.
/// Returns `Err` if the file is missing or empty so callers can distinguish
/// the two cases cleanly.
pub fn read_cookie_file() -> anyhow::Result<String> {
    let path = cookie_file_path();
    if !path.exists() {
        anyhow::bail!("file not found: {}", path.display());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        anyhow::bail!("file is empty");
    }
    Ok(trimmed)
}

/// Create the cookies.txt file with a placeholder comment if it does not
/// already exist.  Called once on first run so the path always exists and
/// the user can open it directly.
pub fn ensure_cookie_file() {
    let path = cookie_file_path();
    if path.exists() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let placeholder = "# Paste your Udemy Cookie header value on the line below and save.\n\
                       # How to get it:\n\
                       #   1. Log in to udemy.com in Firefox / Chrome\n\
                       #   2. DevTools (F12) → Network tab → reload page\n\
                       #   3. Click any request to udemy.com\n\
                       #   4. Request Headers → right-click Cookie → Copy value\n\
                       #   5. Replace this comment with that value and save.\n";
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
