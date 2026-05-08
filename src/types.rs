// src/types.rs – Shared data types across all modules
use std::sync::Arc;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

// ──────────────────────────────────────────────────────────── Udemy API types

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Course {
    pub id: u64,
    pub title: String,
    pub num_published_lectures: Option<u32>,
    pub instructor: String,
}

#[derive(Debug, Clone)]
pub struct Chapter {
    #[allow(dead_code)]
    pub id: u64,
    pub title: String,
    pub object_index: u32,
    pub expanded: bool,
    pub lectures: Vec<Lecture>,
}

#[derive(Debug, Clone)]
pub struct Lecture {
    pub id: u64,
    pub title: String,
    pub object_index: u32,
    /// "Video", "Article", "File", "ExternalLink", …
    pub asset_type: String,
    /// Duration in seconds from Udemy's `time_estimation` field
    pub duration_secs: Option<u32>,
}

/// All stream sources we got for one video lecture
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct LectureStream {
    /// HLS master playlist URL
    pub hls_urls: Vec<String>,
    /// Direct MP4 options sorted by height descending
    pub mp4_urls: Vec<(u32, String)>,
    /// Referer to pass when downloading
    pub referer: String,
}

// ──────────────────────────────────────────────────────── Download tracking

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub enum DownloadStatus {
    Queued,
    Fetching,
    Downloading,
    Done,
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DownloadItem {
    pub id: u64,
    pub course_title: String,
    pub chapter_title: String,
    pub lecture_title: String,
    pub percent: f64,
    pub status: DownloadStatus,
    /// Final output path (may not exist yet)
    pub output_path: String,
    pub cancel: Arc<CancellationToken>,
}

impl DownloadItem {
    pub fn is_active(&self) -> bool {
        matches!(
            self.status,
            DownloadStatus::Queued | DownloadStatus::Fetching | DownloadStatus::Downloading
        )
    }
}

// ──────────────────────────────────────────────────────────────── App events

/// Messages sent from background tokio tasks back to the main event loop.
#[allow(dead_code)]
pub enum AppEvent {
    AuthFailed(String),
    CoursesLoaded(Vec<Course>),
    CurriculumLoaded { course_id: u64, chapters: Vec<Chapter> },
    /// Register a download in the UI *before* the download task starts.
    QueueDownload {
        id: u64,
        course_title: String,
        chapter_title: String,
        lecture_title: String,
        output_path: String,
        cancel: std::sync::Arc<tokio_util::sync::CancellationToken>,
    },
    DownloadProgress { id: u64, percent: f64 },
    DownloadDone { id: u64 },
    DownloadFailed { id: u64, error: String },
    StatusMsg(String),
    Error(String),
}

// ──────────────────────────────────────────────────────── App-state machine

#[derive(Debug, Clone, PartialEq)]
pub enum AppScreen {
    Login,
    Courses,
    Curriculum,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ActiveTab {
    Courses,
    Downloads,
}
