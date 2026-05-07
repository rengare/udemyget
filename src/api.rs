// src/api.rs – Udemy API client.
//
// Auth: cookie-only, confirmed by three live browser captures:
//   POST /api/2024-01/graphql/
//   POST /api-2.0/visits/me/datadog-increment-logs/
//   GET  /api-2.0/shopping-carts/me/
//
// Critical discovery from the third capture:
//   Every /api-2.0/ REST call also sends X-Udemy-Cache-* request headers
//   derived 1:1 from the ud_cache_* cookies.  Without them Udemy returns 403.

use anyhow::{anyhow, Context};
use rquest::header::{self, HeaderMap, HeaderName, HeaderValue};
use rquest_util::Emulation;
use serde_json::Value;

use crate::types::{Chapter, Course, Lecture, LectureStream};

const BASE: &str = "https://www.udemy.com/api-2.0";
const UA:   &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:150.0) Gecko/20100101 Firefox/150.0";

// ── Build reqwest client ──────────────────────────────────────────────────

fn make_client(cookie_str: &str) -> anyhow::Result<rquest::Client> {
    let cookie_header = if cookie_str.contains('=') {
        cookie_str.to_string()
    } else {
        format!("access_token={cookie_str}")
    };

    let mut headers = HeaderMap::new();

    // ── Cookie ──────────────────────────────────────────────────────────
    match HeaderValue::from_str(&cookie_header) {
        Ok(v)  => { headers.insert(header::COOKIE, v); }
        Err(_) => return Err(anyhow!(
            "Cookie string contains characters invalid in an HTTP header.\n\
             Copy only the Cookie header value from DevTools, not the full row."
        )),
    }

    // ── Standard browser headers (confirmed present in all three captures) ──
    headers.insert(header::USER_AGENT,      HeaderValue::from_static(UA));
    headers.insert(header::ACCEPT,          HeaderValue::from_static("application/json, text/plain, */*"));
    headers.insert(header::ACCEPT_LANGUAGE, HeaderValue::from_static("en-US"));
    headers.insert(header::ORIGIN,          HeaderValue::from_static("https://www.udemy.com"));
    headers.insert(header::REFERER,         HeaderValue::from_static("https://www.udemy.com/"));
    headers.insert(
        "X-Requested-With",
        HeaderValue::from_static("XMLHttpRequest"),
    );

    // ── X-Udemy-Cache-* headers ──────────────────────────────────────────
    // The third capture (GET /api-2.0/shopping-carts/me/) shows that Udemy's
    // REST API expects these headers, built from the ud_cache_* cookies:
    //   ud_cache_brand=PLen_US  →  X-Udemy-Cache-Brand: PLen_US
    //   ud_cache_logged_in=1    →  X-Udemy-Cache-Logged-In: 1
    //   ... etc.
    // Without them the server returns 403.
    inject_cache_headers(cookie_str, &mut headers);

    Ok(rquest::Client::builder()
        .emulation(Emulation::Firefox136)
        .default_headers(headers)
        .timeout(std::time::Duration::from_secs(30))
        .build()?)
}

/// Parse every `ud_cache_<key>=<val>` cookie and emit the matching
/// `x-udemy-cache-<key-with-dashes>` request header.
fn inject_cache_headers(cookie_str: &str, headers: &mut HeaderMap) {
    for part in cookie_str.split(';') {
        let part = part.trim();
        let Some(rest) = part.strip_prefix("ud_cache_") else { continue };
        let Some(eq) = rest.find('=')  else { continue };

        let key = &rest[..eq];            // e.g. "marketplace_country"
        let val =  rest[eq + 1..].trim_matches('"'); // e.g. "PL"

        // "marketplace_country" → "marketplace-country"
        let suffix = key.replace('_', "-");
        // HTTP header names are case-insensitive; lowercase is canonical in HTTP/2
        // but the browser sends title-case — both work fine with reqwest.
        let hdr_name = format!("x-udemy-cache-{suffix}");

        if let (Ok(name), Ok(v)) = (
            HeaderName::from_bytes(hdr_name.as_bytes()),
            HeaderValue::from_str(val),
        ) {
            headers.insert(name, v);
        }
    }
}

// ── Verify ────────────────────────────────────────────────────────────────

/// Decode the `ud_user_jwt` cookie payload (no signature check needed —
/// we just want the email for the welcome message).
/// Returns `None` if the cookie is absent or malformed.
pub fn decode_jwt_email(cookie_str: &str) -> Option<String> {
    // Extract ud_user_jwt value
    let jwt = cookie_str.split(';').find_map(|part| {
        let part = part.trim();
        part.strip_prefix("ud_user_jwt=")
    })?;

    // JWT payload is the middle segment (index 1), base64url-encoded
    let payload_b64 = jwt.split('.').nth(1)?;

    // base64url → standard base64
    let std_b64: String = payload_b64
        .chars()
        .map(|c| match c { '-' => '+', '_' => '/', c => c })
        .collect();
    // Re-pad to a multiple of 4
    let pad = (4 - std_b64.len() % 4) % 4;
    let padded = format!("{std_b64}{}", "=".repeat(pad));

    let bytes = base64_decode(&padded)?;
    let json: Value = serde_json::from_slice(&bytes).ok()?;
    json.get("email").and_then(|v| v.as_str()).map(str::to_owned)
}

/// Minimal standard-base64 decoder (no external crate).
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let idx = |c: u8| -> Option<u32> {
        TABLE.iter().position(|&t| t == c).map(|p| p as u32)
    };

    let bytes: Vec<u8> = s.bytes().filter(|&b| b != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);

    for chunk in bytes.chunks(4) {
        let a = idx(chunk[0])?;
        let b = idx(chunk[1])?;
        out.push(((a << 2) | (b >> 4)) as u8);
        if chunk.len() > 2 {
            let c = idx(chunk[2])?;
            out.push(((b << 4) | (c >> 2)) as u8);
            if chunk.len() > 3 {
                let d = idx(chunk[3])?;
                out.push(((c << 6) | d) as u8);
            }
        }
    }
    Some(out)
}

/// Fetch all enrolled courses, handling pagination automatically.
pub async fn fetch_courses(token: &str) -> anyhow::Result<Vec<Course>> {
    let client = make_client(token)?;
    let mut courses: Vec<Course> = Vec::new();
    let mut url = format!(
        "{}/users/me/subscribed-courses/\
         ?ordering=-last_accessed\
         &page_size=100\
         &fields[course]=id,title,num_published_lectures",
        BASE
    );

    loop {
        let resp = client.get(&url).send().await.context("fetching courses")?;
        match resp.status().as_u16() {
            401 | 403 => return Err(anyhow!(
                "HTTP {} — Udemy rejected the cookie.\n\
                 The cookies.txt file may be outdated (cookies expire after some hours).\n\
                 Please copy a fresh Cookie string from DevTools and update the file.",
                resp.status()
            )),
            s if s != 200 => return Err(anyhow!("courses API returned HTTP {s}")),
            _ => {}
        }
        let json: Value = resp.json().await.context("parsing courses")?;

        if let Some(results) = json.get("results").and_then(|v| v.as_array()) {
            for item in results {
                let id = item.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                let title = item
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown")
                    .to_string();
                let num = item
                    .get("num_published_lectures")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
                courses.push(Course { id, title, num_published_lectures: num });
            }
        }

        // Follow pagination
        match json.get("next").and_then(|v| v.as_str()) {
            Some(next) if !next.is_empty() => url = next.to_string(),
            _ => break,
        }
    }

    Ok(courses)
}

/// Fetch the curriculum (chapters + lectures) for a course.
pub async fn fetch_curriculum(
    token: &str,
    course_id: u64,
) -> anyhow::Result<Vec<Chapter>> {
    let client = make_client(token)?;
    let url = format!(
        "{}/courses/{}/cached-subscriber-curriculum-items/\
         ?fields[asset]=asset_type,time_estimation\
         &fields[chapter]=title,object_index,sort_order\
         &fields[lecture]=title,object_index,asset\
         &page_size=1000",
        BASE, course_id
    );

    let resp = client.get(&url).send().await.context("fetching curriculum")?;
    if !resp.status().is_success() {
        return Err(anyhow!("curriculum API returned HTTP {}", resp.status()));
    }
    let json: Value = resp.json().await.context("parsing curriculum")?;

    let results = json
        .get("results")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("curriculum response has no 'results'"))?;

    let mut chapters: Vec<Chapter> = Vec::new();

    for item in results {
        let class = item
            .get("_class")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match class {
            "chapter" => {
                let id = item.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                let title = item
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Chapter")
                    .to_string();
                let object_index = item
                    .get("object_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                chapters.push(Chapter {
                    id,
                    title,
                    object_index,
                    expanded: false,
                    lectures: Vec::new(),
                });
            }
            "lecture" => {
                let id = item.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                let title = item
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Lecture")
                    .to_string();
                let object_index = item
                    .get("object_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;

                let asset = item.get("asset");
                let asset_type = asset
                    .and_then(|a| a.get("asset_type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown")
                    .to_string();
                let duration_secs = asset
                    .and_then(|a| a.get("time_estimation"))
                    .and_then(|v| v.as_u64())
                    .map(|s| s as u32);

                let lecture = Lecture {
                    id,
                    title,
                    object_index,
                    asset_type,
                    duration_secs,
                };

                if let Some(chapter) = chapters.last_mut() {
                    chapter.lectures.push(lecture);
                }
                // Lectures before the first chapter are silently dropped
            }
            _ => {} // quiz, practice, etc.
        }
    }

    Ok(chapters)
}

/// Fetch the stream URLs for a single video lecture.
pub async fn fetch_lecture_streams(
    token: &str,
    course_id: u64,
    lecture_id: u64,
) -> anyhow::Result<LectureStream> {
    let client = make_client(token)?;
    let url = format!(
        "{}/users/me/subscribed-courses/{}/lectures/{}/\
         ?fields[asset]=asset_type,stream_urls,external_url,\
         slide_urls,download_urls,captions,media_sources,media_license_token",
        BASE, course_id, lecture_id
    );

    let resp = client
        .get(&url)
        // Mimic browser: Referer to the actual lecture page
        .header(
            header::REFERER,
            format!(
                "https://www.udemy.com/course/{}/learn/lecture/{}",
                course_id, lecture_id
            ),
        )
        .send()
        .await
        .context("fetching lecture streams")?;
    if !resp.status().is_success() {
        return Err(anyhow!("lecture API returned HTTP {}", resp.status()));
    }
    let json: Value = resp.json().await.context("parsing lecture streams")?;

    let asset = json
        .get("asset")
        .ok_or_else(|| anyhow!("lecture response has no 'asset'"))?;

    let referer = format!("https://www.udemy.com/course/learn/lecture/{}", lecture_id);

    // ── Collect HLS URL and direct MP4 options ──────────────────────────────

    let mut hls_url: Option<String> = None;
    let mut mp4_urls: Vec<(u32, String)> = Vec::new();

    // Prefer media_sources (usually higher quality / more options)
    if let Some(sources) = asset.get("media_sources").and_then(|v| v.as_array()) {
        for src in sources {
            let typ = src.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let file = src
                .get("src")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if file.is_empty() {
                continue;
            }
            if typ.contains("mpegURL") {
                if hls_url.is_none() {
                    hls_url = Some(file);
                }
            } else if typ.contains("mp4") || typ.contains("video") {
                let height = src
                    .get("res")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<u32>().ok())
                    .or_else(|| src.get("res").and_then(|v| v.as_u64()).map(|n| n as u32))
                    .unwrap_or(0);
                mp4_urls.push((height, file));
            }
        }
    }

    // Fall back to stream_urls if media_sources didn't give us an HLS URL
    if hls_url.is_none() {
        if let Some(video_arr) = asset
            .pointer("/stream_urls/Video")
            .and_then(|v| v.as_array())
        {
            for item in video_arr {
                let typ = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let file = item
                    .get("file")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if file.is_empty() {
                    continue;
                }
                if typ.contains("mpegURL") {
                    if hls_url.is_none() {
                        hls_url = Some(file);
                    }
                } else if typ.contains("mp4") || typ.contains("video") {
                    let height = item
                        .get("label")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(0);
                    mp4_urls.push((height, file));
                }
            }
        }
    }

    // Sort MP4s by height descending (best first)
    mp4_urls.sort_by(|a, b| b.0.cmp(&a.0));

    if hls_url.is_none() && mp4_urls.is_empty() {
        return Err(anyhow!(
            "no downloadable stream found for lecture {} (asset_type: {})",
            lecture_id,
            asset
                .get("asset_type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
        ));
    }

    Ok(LectureStream { hls_url, mp4_urls, referer })
}
