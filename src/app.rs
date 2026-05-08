// src/app.rs – App state + keyboard event handling.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::{mpsc::UnboundedSender, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::api;

/// Global cap on simultaneous lecture downloads (API fetch + file write).
/// Keeps socket + file-descriptor counts well below the OS default ulimit.
const MAX_CONCURRENT_DOWNLOADS: usize = 10;

static DOWNLOAD_SEM: std::sync::OnceLock<Arc<Semaphore>> = std::sync::OnceLock::new();

fn download_sem() -> Arc<Semaphore> {
    DOWNLOAD_SEM
        .get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_DOWNLOADS)))
        .clone()
}
use crate::config::{self, Config};
use crate::downloader;
use crate::types::*;

// ── Shared download ID generator ──────────────────────────────────────────

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

// ── App struct ────────────────────────────────────────────────────────────

pub struct App {
    // ---- State machine
    pub screen: AppScreen,
    pub active_tab: ActiveTab,

    // ---- Config (output_dir; cookie is runtime-only from cookies.txt)
    pub config: Config,
    /// Displayed on the setup screen so the user knows exactly which file to edit.
    pub cookie_file: String,

    // ---- Setup screen (shown when cookies.txt is missing / invalid)
    pub login_error: Option<String>,
    pub loading: bool,

    // ---- Courses tab (grouped by instructor)
    pub course_groups: Vec<CourseGroup>,
    /// Flat visible rows: alternating Group headers and Course entries.
    pub course_rows: Vec<CourseRow>,
    pub course_list_state: ratatui::widgets::ListState,

    // ---- Curriculum tab
    pub current_course: Option<Course>,
    pub curriculum: Vec<Chapter>,
    /// Flat list of all visible rows (chapters + expanded lectures)
    pub visible_rows: Vec<CurriculumRow>,
    pub curriculum_list_state: ratatui::widgets::ListState,

    // ---- Downloads
    pub downloads: Vec<DownloadItem>,
    pub downloads_list_state: ratatui::widgets::ListState,

    // ---- UI
    pub status_msg: Option<String>,
    pub error_msg: Option<String>,
    pub should_quit: bool,
}

#[derive(Clone)]
pub struct CourseGroup {
    pub instructor: String,
    pub courses: Vec<Course>,
    pub expanded: bool,
}

#[derive(Clone)]
pub enum CourseRow {
    Group(usize),           // index into course_groups
    Course(usize, usize),   // (group_idx, course_idx)
}

#[derive(Clone)]
pub enum CurriculumRow {
    Chapter(usize),        // index into curriculum
    Lecture(usize, usize), // (chapter_idx, lecture_idx)
}

impl App {
    pub fn new() -> Self {
        // Create cookies.txt placeholder if it doesn't exist yet
        config::ensure_cookie_file();

        let config = config::load();
        let cookie_file = config::cookie_file_display();

        Self {
            screen: AppScreen::Login, // auto_init will advance past this if cookie is valid
            active_tab: ActiveTab::Courses,
            config,
            cookie_file,
            login_error: None,
            loading: false,
            course_groups: Vec::new(),
            course_rows: Vec::new(),
            course_list_state: ratatui::widgets::ListState::default(),
            current_course: None,
            curriculum: Vec::new(),
            visible_rows: Vec::new(),
            curriculum_list_state: ratatui::widgets::ListState::default(),
            downloads: Vec::new(),
            downloads_list_state: ratatui::widgets::ListState::default(),
            status_msg: None,
            error_msg: None,
            should_quit: false,
        }
    }

    /// Read cookies.txt and immediately fetch enrolled courses.
    /// If the file is missing/invalid, shows the setup screen.
    /// Combines what used to be two round-trips (verify + fetch) into one.
    pub fn auto_init(&mut self, tx: UnboundedSender<AppEvent>) {
        match config::read_cookie_file() {
            Ok(cookie) => {
                self.config.cookie = cookie.clone();
                self.loading = true;
                self.login_error = None;

                // Extract email from ud_user_jwt payload (no API call needed)
                let email = api::decode_jwt_email(&cookie)
                    .unwrap_or_else(|| "your account".to_string());
                self.status_msg = Some(format!("Loading courses for {email}…"));

                tokio::spawn(async move {
                    match api::fetch_courses(&cookie).await {
                        Ok(courses) => { tx.send(AppEvent::CoursesLoaded(courses)).ok(); }
                        Err(e)      => { tx.send(AppEvent::AuthFailed(e.to_string())).ok(); }
                    }
                });
            }
            Err(e) => {
                self.loading = false;
                self.login_error = Some(format!("cookies.txt: {e}"));
            }
        }
    }

    // ── Keyboard handler (returns true to quit) ───────────────────────────

    pub fn handle_key(&mut self, key: KeyEvent, tx: UnboundedSender<AppEvent>) -> bool {
        // Global: Ctrl-C to quit from anywhere
        if matches!(
            key,
            KeyEvent {
                code: KeyCode::Char('c') | KeyCode::Char('C'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
        ) {
            return true;
        }

        match self.screen {
            AppScreen::Login => self.handle_key_setup(key, tx),
            AppScreen::Courses => {
                if self.active_tab == ActiveTab::Downloads {
                    self.handle_key_downloads(key);
                } else {
                    self.handle_key_courses(key, tx);
                }
            }
            AppScreen::Curriculum => {
                if self.active_tab == ActiveTab::Downloads {
                    self.handle_key_downloads(key);
                } else {
                    self.handle_key_curriculum(key, tx);
                }
            }
        }

        false
    }

    // ── Per-screen key handlers ───────────────────────────────────────────

    /// Setup screen: the only action is R to retry loading the cookie file.
    fn handle_key_setup(&mut self, key: KeyEvent, tx: UnboundedSender<AppEvent>) {
        match key.code {
            KeyCode::Char('r') | KeyCode::Char('R') => {
                if !self.loading {
                    self.auto_init(tx);
                }
            }
            KeyCode::Char('q') | KeyCode::Char('Q') => {
                self.should_quit = true;
            }
            _ => {}
        }
    }

    fn handle_key_courses(&mut self, key: KeyEvent, tx: UnboundedSender<AppEvent>) {
        match key.code {
            KeyCode::Tab => self.toggle_tab(),
            KeyCode::Char('q') | KeyCode::Char('Q') => { self.should_quit = true; }
            KeyCode::Up   | KeyCode::Char('k') => self.courses_up(),
            KeyCode::Down | KeyCode::Char('j') => self.courses_down(),
            KeyCode::Enter => self.courses_enter(tx),
            KeyCode::Char('d') => self.download_selected_course(tx),
            KeyCode::Char('r') => self.refresh_courses(tx),
            KeyCode::Char('L') => {
                self.config.cookie.clear();
                self.course_groups.clear();
                self.course_rows.clear();
                self.login_error = None;
                self.screen = AppScreen::Login;
            }
            _ => {}
        }
    }

    fn handle_key_curriculum(&mut self, key: KeyEvent, tx: UnboundedSender<AppEvent>) {
        match key.code {
            KeyCode::Tab => self.toggle_tab(),
            KeyCode::Char('q') | KeyCode::Char('Q') => { self.should_quit = true; }
            KeyCode::Up   | KeyCode::Char('k') => self.curriculum_up(),
            KeyCode::Down | KeyCode::Char('j') => self.curriculum_down(),
            KeyCode::Enter | KeyCode::Char(' ') => self.toggle_chapter_expand(),
            KeyCode::Char('d') => self.download_selected_lecture(tx),
            KeyCode::Char('a') => self.download_all_lectures(tx),
            KeyCode::Esc | KeyCode::Char('b') => {
                self.screen = AppScreen::Courses;
                self.curriculum.clear();
                self.visible_rows.clear();
                self.current_course = None;
            }
            _ => {}
        }
    }

    fn handle_key_downloads(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab => self.toggle_tab(),
            KeyCode::Char('q') | KeyCode::Char('Q') => { self.should_quit = true; }
            KeyCode::Up   | KeyCode::Char('k') => self.downloads_up(),
            KeyCode::Down | KeyCode::Char('j') => self.downloads_down(),
            KeyCode::Char('c') | KeyCode::Delete => self.cancel_selected_download(),
            KeyCode::Char('x') => self.clear_finished_downloads(),
            _ => {}
        }
    }

    // ── Navigation helpers ────────────────────────────────────────────────

    fn toggle_tab(&mut self) {
        self.active_tab = match self.active_tab {
            ActiveTab::Courses   => ActiveTab::Downloads,
            ActiveTab::Downloads => ActiveTab::Courses,
        };
    }

    fn courses_up(&mut self) {
        let sel = self.course_list_state.selected().unwrap_or(0);
        if sel > 0 { self.course_list_state.select(Some(sel - 1)); }
    }

    fn courses_down(&mut self) {
        let sel = self.course_list_state.selected().unwrap_or(0);
        if sel + 1 < self.course_rows.len() {
            self.course_list_state.select(Some(sel + 1));
        }
    }

    fn curriculum_up(&mut self) {
        let sel = self.curriculum_list_state.selected().unwrap_or(0);
        if sel > 0 { self.curriculum_list_state.select(Some(sel - 1)); }
    }

    fn curriculum_down(&mut self) {
        let sel = self.curriculum_list_state.selected().unwrap_or(0);
        if sel + 1 < self.visible_rows.len() {
            self.curriculum_list_state.select(Some(sel + 1));
        }
    }

    fn downloads_up(&mut self) {
        let sel = self.downloads_list_state.selected().unwrap_or(0);
        if sel > 0 { self.downloads_list_state.select(Some(sel - 1)); }
    }

    fn downloads_down(&mut self) {
        let sel = self.downloads_list_state.selected().unwrap_or(0);
        if sel + 1 < self.downloads.len() {
            self.downloads_list_state.select(Some(sel + 1));
        }
    }

    fn toggle_chapter_expand(&mut self) {
        let sel = self.curriculum_list_state.selected().unwrap_or(0);
        if let Some(CurriculumRow::Chapter(ci)) = self.visible_rows.get(sel).cloned() {
            self.curriculum[ci].expanded = !self.curriculum[ci].expanded;
            self.rebuild_visible_rows();
        }
    }

    pub fn rebuild_visible_rows(&mut self) {
        self.visible_rows.clear();
        for (ci, ch) in self.curriculum.iter().enumerate() {
            self.visible_rows.push(CurriculumRow::Chapter(ci));
            if ch.expanded {
                for (li, _) in ch.lectures.iter().enumerate() {
                    self.visible_rows.push(CurriculumRow::Lecture(ci, li));
                }
            }
        }
        let max = self.visible_rows.len().saturating_sub(1);
        let sel = self.curriculum_list_state.selected().unwrap_or(0).min(max);
        self.curriculum_list_state.select(Some(sel));
    }

    // ── Actions ───────────────────────────────────────────────────────────

    // -- Course group helpers --

    pub fn rebuild_course_rows(&mut self) {
        self.course_rows.clear();
        for (gi, group) in self.course_groups.iter().enumerate() {
            self.course_rows.push(CourseRow::Group(gi));
            if group.expanded {
                for (ci, _) in group.courses.iter().enumerate() {
                    self.course_rows.push(CourseRow::Course(gi, ci));
                }
            }
        }
        let max = self.course_rows.len().saturating_sub(1);
        let sel = self.course_list_state.selected().unwrap_or(0).min(max);
        if !self.course_rows.is_empty() {
            self.course_list_state.select(Some(sel));
        }
    }

    /// Enter on a Group expands/collapses it; Enter on a Course opens curriculum.
    fn courses_enter(&mut self, tx: UnboundedSender<AppEvent>) {
        let sel = self.course_list_state.selected().unwrap_or(0);
        match self.course_rows.get(sel).cloned() {
            Some(CourseRow::Group(gi)) => {
                self.course_groups[gi].expanded = !self.course_groups[gi].expanded;
                self.rebuild_course_rows();
            }
            Some(CourseRow::Course(gi, ci)) => self.open_curriculum_for(gi, ci, tx),
            None => {}
        }
    }

    fn open_curriculum_for(&mut self, gi: usize, ci: usize, tx: UnboundedSender<AppEvent>) {
        let course = &self.course_groups[gi].courses[ci];
        let course_id = course.id;
        let cookie = self.config.cookie.clone();
        self.loading = true;
        self.status_msg = Some(format!("Loading \"{}\"…", course.title));
        tokio::spawn(async move {
            match api::fetch_curriculum(&cookie, course_id).await {
                Ok(chapters) => { tx.send(AppEvent::CurriculumLoaded { course_id, chapters }).ok(); }
                Err(e)       => { tx.send(AppEvent::Error(e.to_string())).ok(); }
            }
        });
    }

    fn open_curriculum(&mut self, tx: UnboundedSender<AppEvent>) {
        self.courses_enter(tx);
    }

    fn refresh_courses(&mut self, tx: UnboundedSender<AppEvent>) {
        self.loading = true;
        let cookie = self.config.cookie.clone();
        tokio::spawn(async move {
            match api::fetch_courses(&cookie).await {
                Ok(c) => { tx.send(AppEvent::CoursesLoaded(c)).ok(); }
                Err(e) => { tx.send(AppEvent::Error(e.to_string())).ok(); }
            }
        });
    }

    fn download_selected_course(&mut self, tx: UnboundedSender<AppEvent>) {
        let sel = self.course_list_state.selected().unwrap_or(0);
        let course = match self.course_rows.get(sel).cloned() {
            Some(CourseRow::Course(gi, ci)) => self.course_groups[gi].courses[ci].clone(),
            Some(CourseRow::Group(gi)) => {
                self.status_msg = Some(format!(
                    "Press Enter to expand '{}', then select a course",
                    self.course_groups[gi].instructor
                ));
                return;
            }
            None => return,
        };
        let cookie = self.config.cookie.clone();
        let output_dir = self.config.output_dir.clone();
        let tx2 = tx.clone();

        tokio::spawn(async move {
            let chapters = match api::fetch_curriculum(&cookie, course.id).await {
                Ok(c) => c,
                Err(e) => { tx2.send(AppEvent::Error(format!("curriculum: {e}"))).ok(); return; }
            };
            for chapter in &chapters {
                for lecture in &chapter.lectures {
                    if lecture.asset_type != "Video" { continue; }
                    let out_path = config::lecture_output_path(
                        &output_dir, &course.title,
                        chapter.object_index, &chapter.title,
                        lecture.object_index, &lecture.title,
                    );
                    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                    let cancel = Arc::new(CancellationToken::new());
                    tx2.send(AppEvent::QueueDownload {
                        id,
                        course_title: course.title.clone(),
                        chapter_title: chapter.title.clone(),
                        lecture_title: lecture.title.clone(),
                        output_path: out_path.clone(),
                        cancel: cancel.clone(),
                    }).ok();
                    spawn_download(id, cookie.clone(), course.id, lecture.id,
                                   out_path, cancel, tx2.clone());
                }
            }
        });
    }

    fn download_selected_lecture(&mut self, tx: UnboundedSender<AppEvent>) {
        let sel = self.curriculum_list_state.selected().unwrap_or(0);
        let Some(row) = self.visible_rows.get(sel).cloned() else { return };
        let CurriculumRow::Lecture(ci, li) = row else {
            self.status_msg = Some("Select a lecture (not a chapter heading)".to_string());
            return;
        };

        let chapter = &self.curriculum[ci];
        let lecture = &chapter.lectures[li];
        if lecture.asset_type != "Video" {
            self.status_msg = Some(format!(
                "'{}' is not a video ({})", lecture.title, lecture.asset_type
            ));
            return;
        }

        let course = self.current_course.as_ref().unwrap();
        let out_path = config::lecture_output_path(
            &self.config.output_dir, &course.title,
            chapter.object_index, &chapter.title,
            lecture.object_index, &lecture.title,
        );
        let id = next_id();
        let cancel = Arc::new(CancellationToken::new());

        tx.send(AppEvent::QueueDownload {
            id,
            course_title: course.title.clone(),
            chapter_title: chapter.title.clone(),
            lecture_title: lecture.title.clone(),
            output_path: out_path.clone(),
            cancel: cancel.clone(),
        }).ok();

        spawn_download(id, self.config.cookie.clone(), course.id,
                       lecture.id, out_path, cancel, tx);
    }

    fn download_all_lectures(&mut self, tx: UnboundedSender<AppEvent>) {
        let Some(course) = self.current_course.clone() else { return };
        let cookie = self.config.cookie.clone();
        let output_dir = self.config.output_dir.clone();

        for chapter in &self.curriculum {
            for lecture in &chapter.lectures {
                if lecture.asset_type != "Video" { continue; }
                let out_path = config::lecture_output_path(
                    &output_dir, &course.title,
                    chapter.object_index, &chapter.title,
                    lecture.object_index, &lecture.title,
                );
                let id = next_id();
                let cancel = Arc::new(CancellationToken::new());

                tx.send(AppEvent::QueueDownload {
                    id,
                    course_title: course.title.clone(),
                    chapter_title: chapter.title.clone(),
                    lecture_title: lecture.title.clone(),
                    output_path: out_path.clone(),
                    cancel: cancel.clone(),
                }).ok();

                spawn_download(id, cookie.clone(), course.id,
                               lecture.id, out_path, cancel, tx.clone());
            }
        }

        self.status_msg = Some(format!("Queued all video lectures for \"{}\"", course.title));
        self.active_tab = ActiveTab::Downloads;
    }

    fn cancel_selected_download(&mut self) {
        let sel = self.downloads_list_state.selected().unwrap_or(0);
        if let Some(dl) = self.downloads.get(sel) {
            dl.cancel.cancel();
        }
    }

    fn clear_finished_downloads(&mut self) {
        self.downloads.retain(|d| d.is_active());
        let max = self.downloads.len().saturating_sub(1);
        let sel = self.downloads_list_state.selected().unwrap_or(0).min(max);
        self.downloads_list_state.select(
            if self.downloads.is_empty() { None } else { Some(sel) }
        );
    }

    // ── Background event handler ──────────────────────────────────────────

    pub fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::AuthFailed(msg) => {
                self.loading = false;
                self.login_error = Some(msg);
                self.screen = AppScreen::Login;
            }

            AppEvent::CoursesLoaded(courses) => {
                self.loading = false;
                let total = courses.len();
                self.course_groups = build_course_groups(courses);
                self.rebuild_course_rows();
                if !self.course_rows.is_empty() {
                    self.course_list_state.select(Some(0));
                }
                self.screen = AppScreen::Courses;
                self.status_msg = Some(format!(
                    "{total} courses by {} instructors — Enter expand/open · d download · r refresh",
                    self.course_groups.len()
                ));
            }

            AppEvent::CurriculumLoaded { course_id, chapters } => {
                self.loading = false;
                self.curriculum = chapters;
                if let Some(ch) = self.curriculum.first_mut() {
                    ch.expanded = true;
                }
                self.rebuild_visible_rows();
                self.curriculum_list_state.select(Some(0));
                self.current_course = self.course_groups.iter()
                    .flat_map(|g| g.courses.iter())
                    .find(|c| c.id == course_id)
                    .cloned();
                self.screen = AppScreen::Curriculum;
                let total: usize = self.curriculum.iter().map(|ch| ch.lectures.len()).sum();
                self.status_msg = Some(format!(
                    "{} chapters · {total} lectures — Enter expand · d lecture · a all · b back",
                    self.curriculum.len()
                ));
            }

            AppEvent::QueueDownload { id, course_title, chapter_title,
                                      lecture_title, output_path, cancel } => {
                self.downloads.push(DownloadItem {
                    id, course_title, chapter_title, lecture_title,
                    percent: 0.0, status: DownloadStatus::Queued, output_path, cancel,
                });
                if self.downloads_list_state.selected().is_none() {
                    self.downloads_list_state.select(Some(0));
                }
            }

            AppEvent::DownloadProgress { id, percent } => {
                if let Some(dl) = self.downloads.iter_mut().find(|d| d.id == id) {
                    dl.percent = percent;
                    dl.status = if percent > 0.0 {
                        DownloadStatus::Downloading
                    } else {
                        DownloadStatus::Fetching
                    };
                }
            }

            AppEvent::DownloadDone { id } => {
                if let Some(dl) = self.downloads.iter_mut().find(|d| d.id == id) {
                    dl.percent = 100.0;
                    dl.status = DownloadStatus::Done;
                }
            }

            AppEvent::DownloadFailed { id, error } => {
                if let Some(dl) = self.downloads.iter_mut().find(|d| d.id == id) {
                    dl.status = if error.contains("cancel") {
                        DownloadStatus::Cancelled
                    } else {
                        DownloadStatus::Failed(error)
                    };
                }
            }

            AppEvent::StatusMsg(msg) => { self.status_msg = Some(msg); }

            AppEvent::Error(msg) => {
                self.loading = false;
                self.error_msg = Some(msg);
            }
        }
    }

    // ── Post-auth course fetch ────────────────────────────────────────────

}

// ── Background download task ──────────────────────────────────────────────

fn spawn_download(
    id: u64,
    cookie: String,
    course_id: u64,
    lecture_id: u64,
    output_path: String,
    cancel: Arc<CancellationToken>,
    tx: UnboundedSender<AppEvent>,
) {
    let sem = download_sem();
    tokio::spawn(async move {
        // Acquire a slot before touching the network or filesystem.
        // Keeps concurrent downloads ≤ MAX_CONCURRENT_DOWNLOADS so we
        // never exhaust file descriptors or overwhelm the Udemy API.
        let _permit = match sem.acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                tx.send(AppEvent::DownloadFailed {
                    id,
                    error: "download semaphore closed".to_string(),
                }).ok();
                return;
            }
        };
        // _permit is held for the lifetime of this task and released on drop.

        if cancel.is_cancelled() {
            tx.send(AppEvent::DownloadFailed { id, error: "cancelled".to_string() }).ok();
            return;
        }

        tx.send(AppEvent::DownloadProgress { id, percent: 0.0 }).ok();
        tx.send(AppEvent::StatusMsg(format!("Fetching stream for lecture {lecture_id}…"))).ok();

        let stream = match api::fetch_lecture_streams(&cookie, course_id, lecture_id).await {
            Ok(s) => s,
            Err(e) => {
                tx.send(AppEvent::DownloadFailed { id, error: e.to_string() }).ok();
                return;
            }
        };

        if cancel.is_cancelled() {
            tx.send(AppEvent::DownloadFailed { id, error: "cancelled".to_string() }).ok();
            return;
        }

        let (ptx, mut prx) = tokio::sync::mpsc::channel::<f64>(64);
        let tx2 = tx.clone();
        tokio::spawn(async move {
            while let Some(pct) = prx.recv().await {
                tx2.send(AppEvent::DownloadProgress { id, percent: pct }).ok();
            }
        });

        match downloader::download_lecture(&stream, &output_path, ptx, cancel).await {
            Ok(_)  => { tx.send(AppEvent::DownloadDone { id }).ok(); }
            Err(e) => { tx.send(AppEvent::DownloadFailed { id, error: e.to_string() }).ok(); }
        }
        // _permit dropped here — frees a slot for the next queued download.
    });
}

// ── Build course groups (sorted alphabetically by instructor) ─────────────

fn build_course_groups(courses: Vec<Course>) -> Vec<CourseGroup> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<String, Vec<Course>> = BTreeMap::new();
    for course in courses {
        map.entry(course.instructor.clone())
            .or_default()
            .push(course);
    }
    map.into_iter()
        .map(|(instructor, courses)| CourseGroup { instructor, courses, expanded: false })
        .collect()
}
