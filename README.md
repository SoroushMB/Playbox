# PLAYBOX

A high-performance, minimalist LAN video streaming server built with **100% Rust Standard Library (`std`)** — **zero external dependencies**.

---

## Features

- **Zero Crates / Zero Dependencies**: Pure Rust `std` only. Compiles directly with `cargo build --release`.
- **Drop & Run**: Place the standalone binary `playbox` in any folder, or put it in your system `$PATH` (`/usr/local/bin`) to stream any directory with `playbox .`.
- **Automatic LAN IP Discovery & Terminal QR**: Detects your machine's local Wi-Fi / Ethernet IP using zero-packet kernel socket routing and renders a scan-ready QR code right in your terminal.
- **Port Auto-Fallback**: Defaults to port `8080`, and automatically increments (`8081`...`8105`) if occupied.
- **RFC 7233 Byte-Range Streaming**: Native HTTP 206 Partial Content support for instantaneous video scrubbing and seeking.
- **Companion Subtitle Support (.srt / .vtt)**:
  - Automatically pairs `.srt` and `.vtt` subtitles matching the video file stem (e.g. `Movie.en.srt`, `Movie_fa.vtt`).
  - Converts `.srt` subtitles to WebVTT on the fly for native browser `<track>` playback.
  - Video cards display a sleek `[CC]` badge when subtitles are available.
- **Optional LAN PIN Authentication**:
  - Protect shared dorms, offices, or home networks with `--pin <CODE>`.
  - Authenticates via session cookie, `X-Playbox-PIN` header, or URL parameter.
- **Thumbnail Generation**:
  - **Server-side**: Automatically invokes system `ffmpeg` (cached in a hidden `.playbox_thumbs/` directory).
  - **Client-side Fallback**: If `ffmpeg` is absent, the browser extracts frames using an HTML5 `<canvas>` snapshot.
- **UPnP / DLNA Local Discovery**: Built-in SSDP responder on UDP port 1900 with ContentDirectory and ConnectionManager XML services for smart TVs and VLC.
- **Minimalist Black & White UI**:
  - Pure `#000000` monochrome aesthetic with **JetBrains Mono** typography.
  - Smooth lazy loading with blur-up fade-in transitions via `IntersectionObserver`.
  - Responsive media grid with instant real-time search & filter.
  - In-browser video player modal with auto-resumed playback position (`localStorage`).
  - Keyboard shortcuts:
    - `Space` / `K` — Play / Pause
    - `Left` / `Right` / `J` / `L` — Seek ±5s
    - `F` — Fullscreen toggle
    - `M` — Mute / Unmute
    - `Esc` — Close player
    - `/` — Focus search filter

---

## Quick Start

### 1. Build and Run
```bash
# Clone the repository
git clone https://github.com/SoroushMB/Playbox.git
cd Playbox

# Build release binary
cargo build --release

# Run on current directory (port 8080)
cargo run --release

# Or run with custom directory, port, or PIN:
cargo run --release -- /path/to/movies 9000 --pin 1234
```

### 2. Make it globally accessible anywhere
```bash
cargo install --path .
```
Now, simply `cd` into any folder with videos and run:
```bash
playbox
```

### 3. Usage Options
```text
USAGE:
  playbox [OPTIONS] [DIR] [PORT]

OPTIONS:
  -p, --port <PORT>         Port to bind (default: 8080, with fallback)
      --pin <CODE>          Require PIN authentication for web access
  -h, --help                Show this help message
```
