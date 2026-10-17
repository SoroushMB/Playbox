# PLAYBOX

A high-performance, minimalist LAN video streaming server built with **100% Rust Standard Library (`std`)** — **zero external crates or dependencies**.

---

## Features

- **Zero Crates / Zero Dependencies**: Pure Rust `std` only. Compiles directly with `rustc -O main.rs -o playbox`.
- **Drop & Run**: Place the standalone binary `playbox` in any folder, or put it in your system `$PATH` (`/usr/local/bin`) to stream any directory with `playbox .`.
- **Automatic LAN IP Discovery**: Detects your machine's local Wi-Fi / Ethernet IP using zero-packet kernel socket routing so any phone, tablet, or laptop on the same network can connect.
- **Port Auto-Fallback**: Defaults to port `8080`, and automatically increments (`8081`...`8105`) if occupied.
- **HTTP 206 Partial Content (Range Requests)**: Native RFC 7233 byte-range streaming for seeking and scrubbing without buffering the full file into memory.
- **Thumbnail Generation**:
  - **Server-side**: Automatically invokes system `ffmpeg` (cached in a hidden `.playbox_thumbs/` directory).
  - **Client-side Fallback**: If `ffmpeg` is absent, the browser captures the first frame using an HTML5 `<canvas>` snapshot.
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

### 1. Run the pre-compiled binary
```bash
# Stream current folder on default port 8080:
./playbox

# Or stream a specific folder / port:
./playbox /path/to/movies 3000
```

### 2. Make it globally accessible anywhere
```bash
sudo cp playbox /usr/local/bin/
```
Now, simply `cd` into any folder with videos and run:
```bash
playbox
```

### 3. Recompiling from source
No `cargo` needed:
```bash
rustc -O main.rs -o playbox && strip playbox
```
Binary size is only **~575 KB** with all HTML, CSS, and client-side JavaScript embedded inside.
# Playbox
