# udemyget

A terminal UI application for downloading your owned Udemy courses, written in Rust.

> Created with [pi.dev](https://pi.dev) — an AI coding agent.
>
> Download engine adapted from [omniget](https://github.com/tonhowtf/omniget) by tonhowtf —
> the HLS segment downloader (AES-128 decryption, parallel fetch, ordered write) and the
> parallel chunked direct-MP4 downloader were studied and ported from its Rust core.

---

## Features

- Browse all your enrolled Udemy courses
- Expand course curriculum — chapters and lectures
- Download individual lectures or an entire course with one key
- HLS streams with AES-128 decryption and parallel segment download
- Direct MP4 download with parallel chunked range requests
- At most 10 concurrent lecture downloads (prevents file-descriptor exhaustion)
- Cookie-file authentication — no credentials stored in the app
- Firefox 136 TLS fingerprint via [rquest](https://github.com/0x676e67/rquest) so
  Cloudflare's `cf_clearance` check passes
- Proxy support: set `HTTPS_PROXY=http://host:port` (standard env var, no config needed)

---

## Installation

### Prerequisites

| Tool | Notes |
|------|-------|
| Rust 1.75+ | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` |
| C compiler | GCC / Clang (Linux/macOS) or MSVC (Windows) — needed to compile BoringSSL |
| NASM | Required on Windows for BoringSSL (`winget install nasm`) |

### Build

```bash
git clone https://github.com/youruser/udemyget
cd udemyget
cargo build --release
# binary at ./target/release/udemyget
```

---

## Authentication

Udemy authenticates via browser cookies. udemyget reads them from a plain-text file:

| Platform | Path |
|----------|------|
| Linux    | `~/.config/udemyget/cookies.txt` |
| macOS    | `~/Library/Application Support/udemyget/cookies.txt` |
| Windows  | `~\AppData\Roaming\udemyget\cookies.txt` |

The file is created automatically on first run with placeholder instructions.

### Getting the cookie string

1. Log in to [udemy.com](https://www.udemy.com) in Firefox or Chrome
2. Open DevTools (`F12`) → **Network** tab → reload the page
3. Click any request to `udemy.com`
4. In **Request Headers** find `Cookie` → right-click → **Copy value**
5. Replace the entire contents of `cookies.txt` with the copied value and save

> **Note:** Cookies expire after a few hours of inactivity. If you get a 403, refresh
> `cookies.txt` with a new copy from DevTools.

---

## Usage

```bash
./target/release/udemyget
```

### Keyboard shortcuts

| Screen | Key | Action |
|--------|-----|--------|
| Setup | `r` | Reload `cookies.txt` after editing |
| Courses | `↑` / `↓` or `j` / `k` | Navigate list |
| Courses | `Enter` | Open course curriculum |
| Courses | `d` | Download entire course |
| Courses | `r` | Refresh course list |
| Courses | `L` | Log out (clears cookie from memory, re-shows setup) |
| Curriculum | `↑` / `↓` or `j` / `k` | Navigate |
| Curriculum | `Enter` or `Space` | Expand / collapse chapter |
| Curriculum | `d` | Download selected lecture |
| Curriculum | `a` | Download all video lectures in course |
| Curriculum | `b` or `Esc` | Back to course list |
| Downloads | `c` or `Del` | Cancel selected download |
| Downloads | `x` | Clear finished downloads |
| Any | `Tab` | Switch between Courses and Downloads tabs |
| Any | `Ctrl-C` | Quit |

---

## Downloads

Files are saved to `~/Downloads/Udemy/` by default, organised as:

```
~/Downloads/Udemy/
└── Course Title/
    └── 01. Chapter Title/
        ├── 001. Lecture Title.mp4
        ├── 002. Another Lecture.mp4
        └── …
```

---

## Proxy

Set the standard proxy environment variable before running:

```bash
HTTPS_PROXY=http://127.0.0.1:8080 ./udemyget
# or SOCKS5:
HTTPS_PROXY=socks5://127.0.0.1:1080 ./udemyget
```

---

## How it works

### TLS fingerprinting

Udemy is behind Cloudflare. Cloudflare binds the `cf_clearance` session cookie to the
browser's TLS fingerprint (JA3/JA4 hash). A plain `reqwest` client (OpenSSL/rustls)
has a different fingerprint and gets 403. udemyget uses
[rquest](https://github.com/0x676e67/rquest), a `reqwest` fork backed by BoringSSL
patched to emit exactly Firefox 136's cipher suites, TLS extensions, and HTTP/2
settings — so Cloudflare sees an indistinguishable fingerprint from the browser that
generated the cookie.

### Download engine

Adapted from [omniget](https://github.com/tonhowtf/omniget)'s `omniget-core`:

- **HLS** (`hls_downloader.rs`): fetches the master playlist, selects the best variant
  (up to 1080p), downloads segments in parallel with a `Semaphore`, decrypts AES-128 CBC
  segments in-order via a `BTreeMap` buffer, writes the concatenated TS stream to a
  `.part` file and renames on completion.
- **Direct MP4** (`direct_downloader.rs`): HEAD probe for `Content-Length` +
  `Accept-Ranges`, splits into 10 MiB chunks, downloads in parallel, writes at precise
  offsets into a pre-allocated file.

---

## License

MIT
