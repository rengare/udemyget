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

    // Already downloaded
    if output.exists() {
        let size = std::fs::metadata(output)?.len();
        if size > 0 {
            let _ = progress_tx.send(100.0).await;
            return Ok(size);
        }
    }

    let client = build_client()?;

    // 1. Direct MP4 (no DRM, best quality first)
    for (_, url) in &stream.mp4_urls {
        if cancel.is_cancelled() { return Err(anyhow!("cancelled")); }
        match download_direct_inner(&client, url, output, &stream.referer,
                                   progress_tx.clone(), cancel.clone()).await {
            Ok(bytes) => return Ok(bytes),
            Err(e) => eprintln!("[dl] MP4 failed ({e}), trying next…"),
        }
    }

    // 2. HLS — try each URL; skip DRM-protected streams.
    //    CBCS (/cbcs/ in path) = CMAF Common Encryption (Widevine/FairPlay).
    //    We detect those at the URL level to avoid a round-trip.
    let mut drm_count = 0usize;
    let mut last_err: Option<anyhow::Error> = None;

    for m3u8_url in &stream.hls_urls {
        if cancel.is_cancelled() { return Err(anyhow!("cancelled")); }

        // Fast-path: CBCS URLs are always DRM — no need to fetch the playlist.
        if is_cbcs_url(m3u8_url) {
            drm_count += 1;
            last_err = Some(anyhow!("CMAF/CBCS stream"));
            continue;
        }

        match download_hls_inner(&client, m3u8_url, output, &stream.referer,
                                 progress_tx.clone(), cancel.clone()).await {
            Ok(bytes) => return Ok(bytes),
            Err(e) if is_drm_skip(&e) => {
                drm_count += 1;
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }
    }

    // All sources exhausted — build a clear, actionable error.
    let total = stream.hls_urls.len();
    if drm_count > 0 {
        return Err(anyhow!(
            "All {total} HLS stream(s) are DRM-protected (CMAF/CBCS — \
             Widevine/FairPlay).\n\
             This lecture cannot be downloaded without a DRM license.\n\
             If the course allows offline access the direct MP4 \
             download_urls should be populated; try refreshing cookies.txt."
        ));
    }

    Err(last_err.unwrap_or_else(|| anyhow!("no download source available")))
}

/// True for CMAF/CBCS Common Encryption streams — always DRM-protected,
/// no point fetching the playlist.
fn is_cbcs_url(url: &str) -> bool {
    url.contains("/cbcs/") || url.contains("/cenc/")
}

/// True for errors that mean “this stream uses DRM we can’t handle;
/// skip to the next URL” rather than a real network/IO failure.
fn is_drm_skip(e: &anyhow::Error) -> bool {
    e.to_string().contains("drm:skip")
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
//   • SAMPLE-AES decryption via MPEG-TS packet parsing
//   • Ordered BTreeMap-buffered writing
//   • Parallel segment fetch with Semaphore

#[derive(Clone, Copy, PartialEq)]
enum EncMethod { Aes128, SampleAes }

struct EncryptionInfo {
    method: EncMethod,
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
                let iv = compute_iv(&e.iv, next, media_seq);
                match e.method {
                    EncMethod::Aes128    => decrypt_aes128(e, seg_data, next, media_seq)?,
                    EncMethod::SampleAes => decrypt_sample_aes_ts(&seg_data, &e.key, &iv)?,
                }
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

// -- Encryption info fetch --------------------------------------------------

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
                        let key_url   = resolve_url(m3u8_url, uri);
                        let key_bytes = fetch_bytes(client, &key_url, referer, 3).await?;
                        let iv = key.iv.as_ref().map(|s| parse_hex_iv(s));
                        return Ok(Some(EncryptionInfo {
                            method: EncMethod::Aes128,
                            key: key_bytes,
                            iv,
                        }));
                    }
                }

                m3u8_rs::KeyMethod::SampleAES => {
                    let uri    = key.uri.as_deref().unwrap_or("");
                    let format = key.keyformat.as_deref().unwrap_or("");

                    // FairPlay: Apple-proprietary DRM, requires device
                    // attestation.  URI is skd:// or KEYFORMAT identifies
                    // Apple.  Emit a drm:skip sentinel so download_lecture
                    // can fall through to the next HLS URL.
                    if format.contains("com.apple.streamingkeydelivery")
                        || uri.starts_with("skd://")
                    {
                        return Err(anyhow!(
                            "drm:skip FairPlay (Apple-only) on this HLS stream"
                        ));
                    }

                    // Standard SAMPLE-AES: key is a plain 16-byte AES-128 key
                    // served over HTTPS.  Fetch it and decrypt at TS level.
                    if let Some(uri) = &key.uri {
                        let key_url = resolve_url(m3u8_url, uri);
                        let key_bytes = fetch_bytes(client, &key_url, referer, 3)
                            .await
                            .map_err(|e| anyhow!("drm:skip SAMPLE-AES key fetch: {e}"))?;
                        let iv = key.iv.as_ref().map(|s| parse_hex_iv(s));
                        return Ok(Some(EncryptionInfo {
                            method: EncMethod::SampleAes,
                            key: key_bytes,
                            iv,
                        }));
                    }

                    return Err(anyhow!("drm:skip SAMPLE-AES has no key URI"));
                }

                _ => {}
            }
        }
    }
    Ok(None)
}


// -- SAMPLE-AES MPEG-TS decryption ------------------------------------------
//
// SAMPLE-AES (non-FairPlay) encrypts audio/video samples inside TS packets
// using AES-128-CBC.  The key is fetched from the HLS EXT-X-KEY URI (same
// mechanism as AES-128).  Decryption works at the H.264 NAL-unit / AAC-frame
// level inside reassembled PES packets.
//
// Apple HLS SAMPLE-AES spec rules:
//   Video (H.264): NAL unit types 1 (P-slice) and 5 (IDR) are encrypted.
//                  The first 32 bytes of each NAL unit payload are left clear;
//                  all remaining 16-byte-aligned blocks are AES-128-CBC.
//   Audio (AAC):   Each ADTS frame payload (after the 7/9-byte ADTS header)
//                  is AES-128-CBC encrypted (block-aligned portion only).
//   IV:            Per-segment (from EXT-X-KEY IV or derived from sequence
//                  number), reset at the start of each NAL unit / frame.

const TS_SYNC:   u8    = 0x47;
const TS_SIZE:   usize = 188;
const CLEAR_NAL: usize = 32;  // leading bytes of each NAL unit left unencrypted

/// Decrypt a full SAMPLE-AES MPEG-TS segment.
fn decrypt_sample_aes_ts(data: &[u8], key: &[u8], iv: &[u8; 16]) -> anyhow::Result<Vec<u8>> {
    let key16: &[u8; 16] = key.try_into()
        .map_err(|_| anyhow!("SAMPLE-AES key must be 16 bytes, got {}", key.len()))?;

    // Step 1 — collect raw PES payloads per PID from all TS packets
    let (video_pid, audio_pid) = find_av_pids(data);
    let mut pkt = data.to_vec();

    // Step 2 — walk TS packets, reassemble PES, decrypt samples, write back
    let mut i = 0;
    while i + TS_SIZE <= pkt.len() {
        if pkt[i] != TS_SYNC { i += 1; continue; }

        let pid          = (((pkt[i+1] & 0x1F) as u16) << 8) | pkt[i+2] as u16;
        let pusi         = (pkt[i+1] & 0x40) != 0; // payload_unit_start_indicator
        let adapt        = (pkt[i+3] >> 4) & 0x3;
        let adapt_len    = if adapt & 0x2 != 0 { 1 + pkt[i+4] as usize } else { 0 };
        let payload_off  = i + 4 + adapt_len;
        let payload_end  = i + TS_SIZE;

        if payload_off >= payload_end { i += TS_SIZE; continue; }

        if pid == video_pid {
            let payload = &mut pkt[payload_off..payload_end];
            decrypt_h264_pes_payload(payload, key16, iv, pusi)?;
        } else if pid == audio_pid {
            let payload = &mut pkt[payload_off..payload_end];
            decrypt_aac_pes_payload(payload, key16, iv, pusi)?;
        }

        i += TS_SIZE;
    }

    Ok(pkt)
}

/// Find video and audio PIDs by parsing PAT then PMT.
/// Returns (video_pid, audio_pid); uses 0xFFFF as "not found".
fn find_av_pids(data: &[u8]) -> (u16, u16) {
    // 1. Find PMT PID from PAT (PID 0)
    let mut pmt_pid: u16 = 0xFFFF;
    let mut i = 0;
    'pat: while i + TS_SIZE <= data.len() {
        if data[i] != TS_SYNC { i += 1; continue; }
        let pid  = (((data[i+1] & 0x1F) as u16) << 8) | data[i+2] as u16;
        let pusi = (data[i+1] & 0x40) != 0;
        let adapt = (data[i+3] >> 4) & 0x3;
        let adapt_len = if adapt & 0x2 != 0 { 1 + data[i+4] as usize } else { 0 };
        let off = i + 4 + adapt_len;
        if pid == 0 && pusi && off + 12 < i + TS_SIZE {
            // pointer_field + table_id + section_length + transport_stream_id + ...
            let ptr = data[off] as usize;
            let sec = off + 1 + ptr;
            if sec + 12 <= i + TS_SIZE && data[sec] == 0x00 {
                // PAT section: each entry is 4 bytes [program_number(2) + PMT_PID(2)]
                let sec_len  = (((data[sec+1] & 0x0F) as usize) << 8) | data[sec+2] as usize;
                let entries  = sec + 8; // skip header (8 bytes)
                let entries_end = sec + 3 + sec_len - 4; // -4 for CRC
                let mut e = entries;
                while e + 4 <= entries_end && e + 4 <= data.len() {
                    let prog  = ((data[e] as u16) << 8) | data[e+1] as u16;
                    let ppid  = (((data[e+2] & 0x1F) as u16) << 8) | data[e+3] as u16;
                    if prog != 0 { pmt_pid = ppid; break 'pat; }
                    e += 4;
                }
            }
        }
        i += TS_SIZE;
    }

    if pmt_pid == 0xFFFF { return (0xFFFF, 0xFFFF); }

    // 2. Find video/audio PIDs from PMT
    let mut video_pid: u16 = 0xFFFF;
    let mut audio_pid: u16 = 0xFFFF;
    let mut i = 0;
    while i + TS_SIZE <= data.len() {
        if data[i] != TS_SYNC { i += 1; continue; }
        let pid  = (((data[i+1] & 0x1F) as u16) << 8) | data[i+2] as u16;
        let pusi = (data[i+1] & 0x40) != 0;
        let adapt = (data[i+3] >> 4) & 0x3;
        let adapt_len = if adapt & 0x2 != 0 { 1 + data[i+4] as usize } else { 0 };
        let off = i + 4 + adapt_len;
        if pid == pmt_pid && pusi && off + 12 < i + TS_SIZE {
            let ptr = data[off] as usize;
            let sec = off + 1 + ptr;
            if sec + 12 <= i + TS_SIZE && data[sec] == 0x02 {
                let sec_len   = (((data[sec+1] & 0x0F) as usize) << 8) | data[sec+2] as usize;
                let prog_info = (((data[sec+10] & 0x0F) as usize) << 8) | data[sec+11] as usize;
                let mut e     = sec + 12 + prog_info;
                let end       = sec + 3 + sec_len - 4;
                while e + 5 <= end && e + 5 <= data.len() {
                    let stype    = data[e];
                    let es_pid   = (((data[e+1] & 0x1F) as u16) << 8) | data[e+2] as u16;
                    let es_info  = (((data[e+3] & 0x0F) as usize) << 8) | data[e+4] as usize;
                    // stream types: 0x1B = H.264, 0x24 = H.265, 0x0F/0x11 = AAC
                    match stype {
                        0x1B | 0x24 if video_pid == 0xFFFF => video_pid = es_pid,
                        0x0F | 0x11 if audio_pid == 0xFFFF => audio_pid = es_pid,
                        _ => {}
                    }
                    e += 5 + es_info;
                }
            }
        }
        i += TS_SIZE;
    }

    (video_pid, audio_pid)
}

/// Decrypt H.264 PES payload in-place (SAMPLE-AES NAL unit encryption).
fn decrypt_h264_pes_payload(
    payload: &mut [u8],
    key: &[u8; 16],
    iv: &[u8; 16],
    pusi: bool,
) -> anyhow::Result<()> {
    // Skip PES header if this is the start of a PES packet
    let data_start = if pusi { pes_header_len(payload) } else { 0 };
    let stream = &mut payload[data_start..];

    // Walk H.264 byte stream, find NAL units via start codes
    let mut pos = 0;
    while pos + 4 < stream.len() {
        // Find start code (0x000001 or 0x00000001)
        let (sc_len, found) = if stream[pos..].starts_with(&[0,0,0,1]) {
            (4, true)
        } else if stream[pos..].starts_with(&[0,0,1]) {
            (3, true)
        } else {
            (1, false)
        };
        if !found { pos += 1; continue; }

        let nal_start = pos + sc_len;
        if nal_start >= stream.len() { break; }

        let nal_type = stream[nal_start] & 0x1F;

        // Find end of this NAL unit (next start code or end of stream)
        let nal_end = {
            let mut e = nal_start + 1;
            loop {
                if e + 3 > stream.len() { e = stream.len(); break; }
                if stream[e..].starts_with(&[0,0,1]) { break; }
                if e + 4 <= stream.len() && stream[e..].starts_with(&[0,0,0,1]) { break; }
                e += 1;
            }
            e
        };

        // Only P-slice (1) and IDR (5) NAL units are encrypted
        if nal_type == 1 || nal_type == 5 {
            let payload_start = nal_start + 1;          // skip NAL type byte
            let clear_end     = payload_start + CLEAR_NAL; // first 32 bytes clear
            if clear_end < nal_end {
                let enc_slice = &mut stream[clear_end..nal_end];
                aes128_cbc_decrypt_inplace(enc_slice, key, iv)?;
            }
        }

        pos = nal_end;
    }
    Ok(())
}

/// Decrypt AAC PES payload in-place (SAMPLE-AES ADTS frame encryption).
fn decrypt_aac_pes_payload(
    payload: &mut [u8],
    key: &[u8; 16],
    iv: &[u8; 16],
    pusi: bool,
) -> anyhow::Result<()> {
    let data_start = if pusi { pes_header_len(payload) } else { 0 };
    let stream = &mut payload[data_start..];

    let mut pos = 0;
    while pos + 7 < stream.len() {
        // ADTS sync word: 0xFFF?
        if stream[pos] != 0xFF || (stream[pos+1] & 0xF0) != 0xF0 {
            pos += 1;
            continue;
        }
        let protection   = (stream[pos+1] & 0x01) == 1; // 0 = CRC present
        let header_len   = if protection { 7 } else { 9 };
        let frame_len    = (((stream[pos+3] & 0x03) as usize) << 11)
                         | ((stream[pos+4] as usize) << 3)
                         | ((stream[pos+5] >> 5) as usize);
        if frame_len < header_len || pos + frame_len > stream.len() { break; }

        // Encrypt the raw AAC data after the ADTS header
        let enc_slice = &mut stream[pos+header_len..pos+frame_len];
        if enc_slice.len() >= 16 {
            aes128_cbc_decrypt_inplace(enc_slice, key, iv)?;
        }
        pos += frame_len;
    }
    Ok(())
}

/// Return the length of a PES packet header (variable-length).
fn pes_header_len(pes: &[u8]) -> usize {
    if pes.len() < 9 { return 0; }
    // PES start code (3) + stream_id (1) + packet_length (2) + flags (2) + header_data_length (1)
    let hdr_data_len = pes[8] as usize;
    9 + hdr_data_len
}

/// AES-128-CBC decrypt the block-aligned prefix of `buf` in-place.
/// Trailing bytes that don't form a full 16-byte block are left unchanged
/// (per the SAMPLE-AES spec for audio/video samples).
fn aes128_cbc_decrypt_inplace(buf: &mut [u8], key: &[u8; 16], iv: &[u8; 16]) -> anyhow::Result<()> {
    use aes::cipher::{generic_array::GenericArray, BlockDecrypt, KeyInit};

    let blocks = buf.len() / 16;
    if blocks == 0 { return Ok(()); }

    let cipher = aes::Aes128::new(GenericArray::from_slice(key));
    let mut prev = *iv;

    for b in 0..blocks {
        let off   = b * 16;
        let chunk = &mut buf[off..off + 16];
        let ct    = *GenericArray::from_slice(chunk);
        let mut block = ct;
        cipher.decrypt_block(&mut block);
        // CBC XOR
        for j in 0..16 { chunk[j] = block[j] ^ prev[j]; }
        prev = ct.into();
    }
    Ok(())
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
