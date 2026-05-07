// src/downloader.rs – HLS + direct download engine.
//
// Adapted from omniget's `hls_downloader.rs` and `direct_downloader.rs`.
// Key omniget patterns preserved:
//  • AES-128 CBC decryption for encrypted HLS segments
//  • Parallel segment / chunk download via Semaphore + JoinSet
//  • Ordered segment writing via BTreeMap buffer
//  • Exponential back-off with jitter on retries
//  • .part file → rename on completion

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use futures::StreamExt;
use m3u8_rs::{parse_master_playlist, parse_media_playlist, MasterPlaylist, VariantStream};
// Alias rquest as reqwest so all existing reqwest:: paths compile unchanged.
use rquest as reqwest;
use rquest_util::Emulation;
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::types::LectureStream;

// ── Constants ──────────────────────────────────────────────────────────────

const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// Max concurrent HLS segment downloads
const HLS_MAX_CONCURRENT: usize = 8;
/// Max retries per segment / chunk
const MAX_RETRIES: u32 = 5;
/// Direct download chunk size (10 MiB)
const CHUNK_SIZE: u64 = 10 * 1024 * 1024;
/// Only use parallel chunked download above this threshold
const CHUNK_THRESHOLD: u64 = 10 * 1024 * 1024;
/// Max parallel chunks for direct downloads
const MAX_PARALLEL_CHUNKS: usize = 6;
/// Per-operation timeout
const OP_TIMEOUT: Duration = Duration::from_secs(60);

// ── Public entry point ────────────────────────────────────────────────────

/// Download a lecture using the best available source.
///
/// `progress_tx` receives values in 0..=100 (f64 percent).
/// `cancel` can be triggered externally to abort.
pub async fn download_lecture(
    stream: &LectureStream,
    output_path: &str,
    progress_tx: mpsc::Sender<f64>,
    cancel: Arc<CancellationToken>,
) -> anyhow::Result<u64> {
    let output = Path::new(output_path);
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Skip if already downloaded
    if output.exists() {
        let size = std::fs::metadata(output)?.len();
        if size > 0 {
            let _ = progress_tx.send(100.0).await;
            return Ok(size);
        }
    }

    let client = build_client()?;

    // Prefer a direct MP4 (first = highest quality due to pre-sort)
    if let Some((_, url)) = stream.mp4_urls.first() {
        match download_direct_inner(
            &client,
            url,
            output,
            &stream.referer,
            progress_tx.clone(),
            cancel.clone(),
        )
        .await
        {
            Ok(bytes) => return Ok(bytes),
            Err(e) => {
                // Log and fall through to HLS
                eprintln!("[downloader] direct download failed ({e}), trying HLS…");
            }
        }
    }

    // Fall back to HLS
    if let Some(ref m3u8_url) = stream.hls_url {
        return download_hls_inner(
            &client,
            m3u8_url,
            output,
            &stream.referer,
            progress_tx,
            cancel,
        )
        .await;
    }

    Err(anyhow!("no download source available"))
}

// ── HTTP client ───────────────────────────────────────────────────────────

fn build_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .emulation(Emulation::Firefox136)
        .connect_timeout(Duration::from_secs(20))
        .pool_max_idle_per_host(20)
        .pool_idle_timeout(Duration::from_secs(30))
        .build()?)
}

fn ua_headers(referer: &str) -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    if let Ok(v) = reqwest::header::HeaderValue::from_str(USER_AGENT) {
        h.insert(reqwest::header::USER_AGENT, v);
    }
    if let Ok(v) = reqwest::header::HeaderValue::from_str(referer) {
        h.insert(reqwest::header::REFERER, v);
    }
    h
}

fn part_path(output: &Path) -> PathBuf {
    let mut s = output.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

fn jitter_sleep(attempt: u32) -> Duration {
    let base_ms = 500u64 * (attempt + 1) as u64;
    let jitter = rand::random::<u64>() % (base_ms / 2 + 1);
    Duration::from_millis(base_ms + jitter)
}

// ══════════════════════════════════════════════════════ DIRECT DOWNLOADER

async fn download_direct_inner(
    client: &reqwest::Client,
    url: &str,
    output: &Path,
    referer: &str,
    progress_tx: mpsc::Sender<f64>,
    cancel: Arc<CancellationToken>,
) -> anyhow::Result<u64> {
    // HEAD probe
    let probe = probe_url(client, url, referer).await;

    let use_chunked = probe.accept_ranges
        && probe.content_length.is_some_and(|s| s >= CHUNK_THRESHOLD);

    let pp = part_path(output);

    if use_chunked {
        let total = probe.content_length.unwrap();
        download_chunked(client, url, &pp, total, referer, &progress_tx, &cancel).await?;
    } else {
        download_stream(
            client,
            url,
            &pp,
            0,
            probe.content_length,
            referer,
            &progress_tx,
            &cancel,
        )
        .await?;
    }

    // Verify size if known
    if let Some(expected) = probe.content_length {
        let actual = std::fs::metadata(&pp)?.len();
        if expected > 0 && actual != expected {
            let _ = std::fs::remove_file(&pp);
            return Err(anyhow!(
                "size mismatch: expected {expected} bytes, got {actual}"
            ));
        }
    }

    std::fs::rename(&pp, output)?;
    let size = std::fs::metadata(output)?.len();
    let _ = progress_tx.send(100.0).await;
    Ok(size)
}

struct ProbeResult {
    content_length: Option<u64>,
    accept_ranges: bool,
}

async fn probe_url(client: &reqwest::Client, url: &str, referer: &str) -> ProbeResult {
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        client.head(url).headers(ua_headers(referer)).send(),
    )
    .await;

    match result {
        Ok(Ok(r)) if r.status().is_success() => ProbeResult {
            content_length: r.content_length(),
            accept_ranges: r
                .headers()
                .get("accept-ranges")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.contains("bytes"))
                .unwrap_or(false),
        },
        _ => ProbeResult { content_length: None, accept_ranges: false },
    }
}

async fn download_chunked(
    client: &reqwest::Client,
    url: &str,
    pp: &Path,
    total: u64,
    referer: &str,
    progress_tx: &mpsc::Sender<f64>,
    cancel: &Arc<CancellationToken>,
) -> anyhow::Result<()> {
    // Pre-allocate file
    {
        let f = std::fs::File::create(pp)?;
        f.set_len(total)?;
    }

    let num_chunks = total.div_ceil(CHUNK_SIZE);
    let downloaded = Arc::new(AtomicU64::new(0));
    let sem = Arc::new(Semaphore::new(MAX_PARALLEL_CHUNKS));
    let mut jset = tokio::task::JoinSet::new();

    for i in 0..num_chunks {
        let start = i * CHUNK_SIZE;
        let end = ((i + 1) * CHUNK_SIZE - 1).min(total - 1);
        let client = client.clone();
        let url = url.to_string();
        let pp = pp.to_owned();
        let referer = referer.to_string();
        let sem = sem.clone();
        let dl = downloaded.clone();
        let ptx = progress_tx.clone();
        let ct = cancel.clone();

        jset.spawn(async move {
            let _permit = sem.acquire_owned().await?;
            if ct.is_cancelled() {
                return Err(anyhow!("cancelled"));
            }
            download_chunk(&client, &url, &pp, start, end, total, &dl, &ptx, &referer, &ct).await
        });
    }

    while let Some(res) = jset.join_next().await {
        if cancel.is_cancelled() {
            jset.abort_all();
            let _ = std::fs::remove_file(pp);
            return Err(anyhow!("cancelled"));
        }
        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                jset.abort_all();
                let _ = std::fs::remove_file(pp);
                return Err(e);
            }
            Err(e) => {
                jset.abort_all();
                let _ = std::fs::remove_file(pp);
                return Err(anyhow!("chunk task panicked: {e:?}"));
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn download_chunk(
    client: &reqwest::Client,
    url: &str,
    pp: &Path,
    start: u64,
    end: u64,
    total: u64,
    downloaded: &AtomicU64,
    ptx: &mpsc::Sender<f64>,
    referer: &str,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 0..MAX_RETRIES {
        if cancel.is_cancelled() {
            return Err(anyhow!("cancelled"));
        }
        if attempt > 0 {
            tokio::time::sleep(jitter_sleep(attempt)).await;
        }

        let resp = match tokio::time::timeout(
            OP_TIMEOUT,
            client
                .get(url)
                .headers(ua_headers(referer))
                .header("Range", format!("bytes={start}-{end}"))
                .send(),
        )
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => { last_err = Some(e.into()); continue; }
            Err(_) => { last_err = Some(anyhow!("timeout connecting")); continue; }
        };

        if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            last_err = Some(anyhow!("HTTP {}", resp.status()));
            if resp.status().is_client_error() { break; } // fatal
            continue;
        }

        let bytes = match tokio::time::timeout(OP_TIMEOUT, resp.bytes()).await {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => { last_err = Some(e.into()); continue; }
            Err(_) => { last_err = Some(anyhow!("timeout reading chunk")); continue; }
        };

        // Write at offset
        use std::io::{Seek, Write};
        let mut file = std::fs::OpenOptions::new().write(true).open(pp)?;
        file.seek(std::io::SeekFrom::Start(start))?;
        file.write_all(&bytes)?;

        let prev = downloaded.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        let pct = ((prev + bytes.len() as u64) as f64 / total as f64 * 100.0).min(99.9);
        let _ = ptx.send(pct).await;
        return Ok(());
    }

    Err(last_err.unwrap_or_else(|| anyhow!("chunk failed after {MAX_RETRIES} attempts")))
}

async fn download_stream(
    client: &reqwest::Client,
    url: &str,
    pp: &Path,
    start_offset: u64,
    total: Option<u64>,
    referer: &str,
    ptx: &mpsc::Sender<f64>,
    cancel: &Arc<CancellationToken>,
) -> anyhow::Result<()> {
    let mut req = client.get(url).headers(ua_headers(referer));
    if start_offset > 0 {
        req = req.header("Range", format!("bytes={start_offset}-"));
    }

    let resp = req.send().await?;
    if !resp.status().is_success() && resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(anyhow!("HTTP {}", resp.status()));
    }

    // Detect HTML redirect
    if resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("text/html"))
        .unwrap_or(false)
    {
        return Err(anyhow!("server returned HTML — URL may have expired"));
    }

    use std::io::Write;
    let f = if start_offset > 0 {
        std::fs::OpenOptions::new().append(true).open(pp)?
    } else {
        std::fs::File::create(pp)?
    };
    let mut w = std::io::BufWriter::with_capacity(256 * 1024, f);
    let mut done = start_offset;
    let mut stream = resp.bytes_stream();

    loop {
        if cancel.is_cancelled() {
            w.flush()?;
            return Err(anyhow!("cancelled"));
        }
        match tokio::time::timeout(OP_TIMEOUT, stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                w.write_all(&chunk)?;
                done += chunk.len() as u64;
                if let Some(t) = total {
                    if t > 0 {
                        let _ = ptx.send((done as f64 / t as f64 * 100.0).min(99.9)).await;
                    }
                }
            }
            Ok(Some(Err(e))) => { w.flush()?; return Err(e.into()); }
            Ok(None) => break,
            Err(_) => { w.flush()?; return Err(anyhow!("timeout — no data for 60 s")); }
        }
    }

    w.flush()?;
    Ok(())
}

// ══════════════════════════════════════════════════════════ HLS DOWNLOADER

// Adapted 1-for-1 from omniget's hls_downloader.rs
// Key features preserved:
//   • Master playlist variant selection (≤ requested height)
//   • AES-128 CBC decryption with per-segment IV
//   • Ordered BTreeMap-buffered writing
//   • Parallel segment fetch with Semaphore

struct EncryptionInfo {
    key: Vec<u8>,
    iv: Option<[u8; 16]>,
}

async fn download_hls_inner(
    client: &reqwest::Client,
    m3u8_url: &str,
    output: &Path,
    referer: &str,
    ptx: mpsc::Sender<f64>,
    cancel: Arc<CancellationToken>,
) -> anyhow::Result<u64> {
    if cancel.is_cancelled() {
        return Err(anyhow!("cancelled"));
    }

    let m3u8_text = fetch_text(client, m3u8_url, referer, MAX_RETRIES).await?;
    let bytes = m3u8_text.as_bytes();

    // Try as master playlist first
    if let Ok((_, master)) = parse_master_playlist(bytes) {
        if let Some(variant) = best_variant(&master, 1080) {
            let variant_url = resolve_url(m3u8_url, &variant.uri);
            return download_hls_media(
                client, &variant_url, output, referer, ptx, cancel,
            )
            .await;
        }
    }

    // Otherwise treat as a media playlist directly
    download_hls_media(client, m3u8_url, output, referer, ptx, cancel).await
}

async fn download_hls_media(
    client: &reqwest::Client,
    m3u8_url: &str,
    output: &Path,
    referer: &str,
    ptx: mpsc::Sender<f64>,
    cancel: Arc<CancellationToken>,
) -> anyhow::Result<u64> {
    let text = fetch_text(client, m3u8_url, referer, MAX_RETRIES).await?;
    let (_, playlist) = parse_media_playlist(text.as_bytes())
        .map_err(|e| anyhow!("parse media playlist: {e:?}"))?;

    let total_segs = playlist.segments.len();
    let enc = fetch_enc_info(client, &playlist, m3u8_url, referer).await?;
    let media_seq = playlist.media_sequence;

    let pp = part_path(output);
    if let Some(p) = pp.parent() {
        std::fs::create_dir_all(p)?;
    }

    // Channel for (idx, data) from segment workers → ordered writer
    let (seg_tx, seg_rx) = tokio::sync::mpsc::channel::<(usize, Vec<u8>)>(HLS_MAX_CONCURRENT);

    let writer_pp = pp.clone();
    let writer = tokio::spawn(async move {
        write_segments_ordered(seg_rx, &writer_pp, &enc, media_seq, total_segs).await
    });

    let sem = Arc::new(Semaphore::new(HLS_MAX_CONCURRENT));
    let fail_tok = cancel.child_token();
    let completed = Arc::new(AtomicU64::new(0));

    let segment_urls: Vec<(usize, String)> = playlist
        .segments
        .iter()
        .enumerate()
        .map(|(i, s)| (i, resolve_url(m3u8_url, &s.uri)))
        .collect();

    let client_ref = client;

    futures::stream::iter(segment_urls)
        .map(|(i, url)| {
            let ptx = ptx.clone();
            let seg_tx = seg_tx.clone();
            let fail = fail_tok.clone();
            let sem = sem.clone();
            let done = completed.clone();
            let total = total_segs as u64;

            async move {
                let _permit = sem.acquire().await.unwrap();
                if fail.is_cancelled() {
                    return;
                }
                match fetch_segment(client_ref, &url, referer, MAX_RETRIES, &fail).await {
                    Ok(data) => {
                        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                        let pct = (n as f64 / total as f64 * 100.0).min(99.9);
                        let _ = ptx.send(pct).await;
                        let _ = seg_tx.send((i, data)).await;
                    }
                    Err(_) => fail.cancel(),
                }
            }
        })
        .buffer_unordered(HLS_MAX_CONCURRENT)
        .collect::<()>()
        .await;

    drop(seg_tx);

    let writer_result = writer.await.map_err(|e| anyhow!("writer panic: {e:?}"))??;
    let _ = writer_result; // () returned

    if cancel.is_cancelled() {
        let _ = std::fs::remove_file(&pp);
        return Err(anyhow!("cancelled"));
    }
    if fail_tok.is_cancelled() {
        let _ = std::fs::remove_file(&pp);
        return Err(anyhow!("one or more segments failed"));
    }

    std::fs::rename(&pp, output)?;
    let _ = progress_tx_done(&ptx).await;
    let size = std::fs::metadata(output)?.len();
    Ok(size)
}

async fn progress_tx_done(tx: &mpsc::Sender<f64>) -> anyhow::Result<()> {
    tx.send(100.0).await.ok();
    Ok(())
}

// ── Ordered segment writer ────────────────────────────────────────────────

async fn write_segments_ordered(
    mut rx: tokio::sync::mpsc::Receiver<(usize, Vec<u8>)>,
    pp: &Path,
    enc: &Option<EncryptionInfo>,
    media_seq: u64,
    total: usize,
) -> anyhow::Result<()> {
    use std::io::Write;
    let mut f = std::io::BufWriter::with_capacity(
        256 * 1024,
        std::fs::File::create(pp)?,
    );
    let mut next: usize = 0;
    let mut pending: BTreeMap<usize, Vec<u8>> = BTreeMap::new();

    while let Some((idx, data)) = rx.recv().await {
        pending.insert(idx, data);
        while let Some(seg_data) = pending.remove(&next) {
            let to_write = if let Some(e) = enc {
                decrypt_aes128(e, seg_data, next, media_seq)?
            } else {
                seg_data
            };
            f.write_all(&to_write)?;
            next += 1;
        }
    }

    f.flush()?;

    if next < total {
        return Err(anyhow!("only {next}/{total} segments written"));
    }
    Ok(())
}

// ── AES-128 CBC decryption ────────────────────────────────────────────────

fn decrypt_aes128(
    enc: &EncryptionInfo,
    mut buf: Vec<u8>,
    seg_idx: usize,
    media_seq: u64,
) -> anyhow::Result<Vec<u8>> {
    use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
    type Dec = cbc::Decryptor<aes::Aes128>;

    let iv = compute_iv(&enc.iv, seg_idx, media_seq);
    let dec = Dec::new_from_slices(&enc.key, &iv)
        .map_err(|e| anyhow!("AES init: {e:?}"))?;
    let out = dec
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| anyhow!("AES decrypt: {e:?}"))?;
    Ok(out.to_vec())
}

fn compute_iv(explicit: &Option<[u8; 16]>, seg_idx: usize, media_seq: u64) -> [u8; 16] {
    if let Some(iv) = explicit {
        return *iv;
    }
    let seq = media_seq + seg_idx as u64;
    let mut iv = [0u8; 16];
    iv[8..16].copy_from_slice(&seq.to_be_bytes());
    iv
}

// ── Encryption info fetch ─────────────────────────────────────────────────

async fn fetch_enc_info(
    client: &reqwest::Client,
    playlist: &m3u8_rs::MediaPlaylist,
    m3u8_url: &str,
    referer: &str,
) -> anyhow::Result<Option<EncryptionInfo>> {
    for seg in &playlist.segments {
        if let Some(key) = &seg.key {
            match key.method {
                m3u8_rs::KeyMethod::AES128 => {
                    if let Some(uri) = &key.uri {
                        let key_url = resolve_url(m3u8_url, uri);
                        let key_bytes =
                            fetch_bytes(client, &key_url, referer, 3).await?;
                        let iv = key.iv.as_ref().map(|s| parse_hex_iv(s));
                        return Ok(Some(EncryptionInfo { key: key_bytes, iv }));
                    }
                }
                m3u8_rs::KeyMethod::SampleAES => {
                    return Err(anyhow!(
                        "SAMPLE-AES / FairPlay DRM detected — cannot decrypt"
                    ));
                }
                _ => {}
            }
        }
    }
    Ok(None)
}

fn parse_hex_iv(s: &str) -> [u8; 16] {
    let hex = s.trim_start_matches("0x").trim_start_matches("0X");
    let padded = format!("{:0>32}", hex);
    let mut iv = [0u8; 16];
    for i in 0..16 {
        iv[i] = u8::from_str_radix(&padded[i * 2..i * 2 + 2], 16).unwrap_or(0);
    }
    iv
}

// ── Helpers ───────────────────────────────────────────────────────────────

fn best_variant(master: &MasterPlaylist, max_h: u32) -> Option<&VariantStream> {
    let real: Vec<&VariantStream> =
        master.variants.iter().filter(|v| !v.is_i_frame).collect();
    if real.is_empty() {
        return None;
    }
    let mut sorted = real;
    sorted.sort_by_key(|v| v.resolution.as_ref().map(|r| r.height).unwrap_or(0));

    let mut best: Option<&VariantStream> = None;
    for v in &sorted {
        if v.resolution
            .as_ref()
            .map(|r| r.height <= max_h as u64)
            .unwrap_or(true)
        {
            best = Some(*v);
        }
    }
    best.or_else(|| sorted.first().copied())
}

fn resolve_url(base: &str, rel: &str) -> String {
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return rel.to_string();
    }
    let (base_path, query) = match base.find('?') {
        Some(p) => (&base[..p], Some(&base[p..])),
        None => (base, None),
    };
    let resolved = if let Some(pos) = base_path.rfind('/') {
        format!("{}/{}", &base_path[..pos], rel)
    } else {
        rel.to_string()
    };
    match query {
        Some(q) if !rel.contains('?') => format!("{resolved}{q}"),
        _ => resolved,
    }
}

async fn fetch_text(
    client: &reqwest::Client,
    url: &str,
    referer: &str,
    retries: u32,
) -> anyhow::Result<String> {
    let mut last_err = None;
    for attempt in 0..retries {
        if attempt > 0 {
            tokio::time::sleep(jitter_sleep(attempt)).await;
        }
        match tokio::time::timeout(
            OP_TIMEOUT,
            client.get(url).headers(ua_headers(referer)).send(),
        )
        .await
        {
            Ok(Ok(r)) if r.status().is_success() => {
                match r.text().await {
                    Ok(t) => return Ok(t),
                    Err(e) => last_err = Some(anyhow!(e)),
                }
            }
            Ok(Ok(r)) => last_err = Some(anyhow!("HTTP {}", r.status())),
            Ok(Err(e)) => last_err = Some(anyhow!(e)),
            Err(_) => last_err = Some(anyhow!("timeout")),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("fetch failed after {retries} attempts")))
}

async fn fetch_bytes(
    client: &reqwest::Client,
    url: &str,
    referer: &str,
    retries: u32,
) -> anyhow::Result<Vec<u8>> {
    let mut last_err = None;
    for attempt in 0..retries {
        if attempt > 0 {
            tokio::time::sleep(jitter_sleep(attempt)).await;
        }
        match tokio::time::timeout(
            OP_TIMEOUT,
            client.get(url).headers(ua_headers(referer)).send(),
        )
        .await
        {
            Ok(Ok(r)) if r.status().is_success() => match r.bytes().await {
                Ok(b) => return Ok(b.to_vec()),
                Err(e) => last_err = Some(anyhow!(e)),
            },
            Ok(Ok(r)) => last_err = Some(anyhow!("HTTP {}", r.status())),
            Ok(Err(e)) => last_err = Some(anyhow!(e)),
            Err(_) => last_err = Some(anyhow!("timeout")),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("fetch_bytes failed")))
}

async fn fetch_segment(
    client: &reqwest::Client,
    url: &str,
    referer: &str,
    retries: u32,
    cancel: &CancellationToken,
) -> anyhow::Result<Vec<u8>> {
    let mut last_err = None;
    for attempt in 0..retries {
        if cancel.is_cancelled() {
            return Err(anyhow!("cancelled"));
        }
        if attempt > 0 {
            tokio::time::sleep(jitter_sleep(attempt)).await;
        }
        let result = tokio::time::timeout(
            OP_TIMEOUT,
            client.get(url).headers(ua_headers(referer)).send(),
        )
        .await;

        let resp = match result {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => { last_err = Some(anyhow!(e)); continue; }
            Err(_) => { last_err = Some(anyhow!("timeout")); continue; }
        };

        if !resp.status().is_success() {
            let code = resp.status().as_u16();
            last_err = Some(anyhow!("HTTP {code}"));
            if (400..500).contains(&code) && code != 429 {
                break; // fatal 4xx
            }
            continue;
        }

        match tokio::time::timeout(OP_TIMEOUT, resp.bytes()).await {
            Ok(Ok(b)) => return Ok(b.to_vec()),
            Ok(Err(e)) => last_err = Some(anyhow!(e)),
            Err(_) => last_err = Some(anyhow!("segment read timeout")),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("segment failed after {retries} attempts")))
}
