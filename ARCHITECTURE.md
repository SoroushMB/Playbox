# Playbox: Architecture & Engineering Deep-Dive

Playbox is a minimalist, single-binary local-area network (LAN) video streaming server. It is engineered with **100% Rust Standard Library (`std`)** with **zero external dependencies (`Cargo.lock` has 0 third-party crates)**. 

This document details the architectural decisions, Rust language constructs, kernel-level networking mechanisms, and algorithmic techniques employed throughout the codebase.

---

## 1. Architectural Philosophy: The Zero-Crate Design

Modern Rust web servers typically depend on heavy async runtimes (`tokio`, `hyper`, `axum`) pulling in hundreds of transitive dependencies, resulting in multi-megabyte binaries and prolonged compile times.

Playbox intentionally bypasses external runtimes by leveraging native OS primitives exposed through `std::net`, `std::sync`, and `std::thread`:
- **Binary Footprint:** ~630 KB fully optimized and stripped.
- **Compilation Speed:** < 1 second release builds from a cold cache.
- **Embedded Static Assets:** Single binary distribution via `include_str!`.
- **Portability:** Builds for Linux (GNU/musl), macOS (Universal: x86_64 / arm64), and Windows without C toolchain link dependencies.

---

## 2. Rust Features & Concurrency Primitives

### 2.1 Multi-Producer, Single-Consumer (mpsc) Worker Pool
Instead of spawning a new OS thread per incoming TCP connection (which exposes the server to file descriptor exhaustion during rapid HTTP keep-alive requests), Playbox uses a **fixed-size 16-worker thread pool** feeding from an `mpsc` channel wrapped in an `Arc<Mutex<Receiver<...>>>`:

```rust
let (tx, rx) = mpsc::channel::<(TcpStream, ServerConfig)>();
let rx = Arc::new(Mutex::new(rx));

for _ in 0..16 {
    let rx = Arc::clone(&rx);
    thread::spawn(move || {
        loop {
            let job = {
                let rx = rx.lock().unwrap();
                rx.recv()
            };
            match job {
                Ok((stream, cfg)) => {
                    let _ = handle_connection(stream, cfg);
                }
                Err(_) => break, // Channel closed on server shutdown
            }
        }
    });
}
```

#### Why `rx.lock().unwrap()` is scoped:
The lock acquisition is scoped strictly around `rx.recv()`. As soon as a worker claims a socket off the channel, the mutex guard drops immediately. The expensive work (`handle_connection`) proceeds concurrently across all 16 workers without holding the queue lock.

### 2.2 Shared State & Deduplication via `Arc<Mutex<T>>`
Thumbnail generation via FFmpeg is CPU-intensive. If a web browser loads a grid of 50 video cards, 50 parallel requests for `/api/thumb?path=...` arrive at once.

To prevent running 50 simultaneous FFmpeg processes on the same core:
- A shared `Arc<Mutex<HashSet<String>>>` tracks in-flight generation tasks.
- If a video thumbnail is currently being computed by thread A, thread B immediately returns `204 No Content`, preventing thundering herd CPU spikes.

### 2.3 Compile-Time Resource Inlining (`include_str!`)
The entire frontend (HTML, CSS styling, embedded SVG icons, and vanilla JavaScript) is stored in `src/web/index.html` and bundled directly into the read-only data section (`.rodata`) of the executable at compile time:

```rust
pub const EMBEDDED_HTML: &str = include_str!("web/index.html");
```
This guarantees that the application runs anywhere as a standalone binary without needing an adjacent `public/` or `assets/` folder.

---

## 3. Kernel Networking & Low-Level Tricks

### 3.1 Zero-Packet LAN IP Discovery
Standard methods for finding a local IP address either require platform-specific shell calls (`ifconfig`, `ip route`) or iterating network interfaces using raw sockets (`getifaddrs`).

Playbox uses an OS kernel routing table trick via UDP:
```rust
pub fn get_local_ip() -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}
```
#### How it works:
Calling `connect()` on a UDP socket **does not send any packets over the wire** (UDP is connectionless). Instead, it forces the OS kernel to consult its internal IP routing table to answer: *"If I were to transmit a datagram toward 8.8.8.8, which local network interface and source IP address would be selected?"* The kernel assigns this address to `socket.local_addr()`, yielding the active LAN IP instantly in user space with zero network overhead.

### 3.2 TCP Socket Tuning (`TCP_NODELAY` & Keep-Alive)
Video streaming requires low latency, while video seeking demands rapid TCP response times.

1. **`stream.set_nodelay(true)`:** Disables Nagle's algorithm. By default, Nagle buffers small chunks until an acknowledgment (ACK) returns or a full packet size (MSS) accumulates. Disabling it ensures HTTP headers and the initial frames of video seek requests are flushed to the wire immediately.
2. **`stream.set_read_timeout` / `stream.set_write_timeout`:** Prevents deadlocks from stale or hanging mobile client connections.

### 3.3 Dynamic Port Fallback
If the default port (`8080`) is in use, Playbox will not terminate with `EADDRINUSE`. Instead, it automatically probes candidate ports up to `8080 + 25` sequentially:
```rust
for candidate in DEFAULT_PORT..(DEFAULT_PORT + 25) {
    let addr = format!("0.0.0.0:{}", candidate);
    if let Ok(l) = TcpListener::bind(&addr) {
        bound = Some((l, candidate));
        break;
    }
}
```

---

## 4. Media Streaming & RFC 7233 (HTTP 206 Partial Content)

Modern browsers cannot smoothly play multi-gigabyte video files with standard `HTTP 200 OK` full responses because the user cannot scrub forward without downloading everything in between.

Playbox implements full RFC 7233 byte-range parsing and partial content delivery:

```
Browser Request:
GET /api/stream?path=movie.mp4 HTTP/1.1
Range: bytes=10485760-20971519

Server Response:
HTTP/1.1 206 Partial Content
Content-Type: video/mp4
Content-Range: bytes 10485760-20971519/1073741824
Content-Length: 10485760
Accept-Ranges: bytes
Connection: keep-alive
```

### 4.1 Byte Range Specifier Support
The parser handles all three standardized byte range forms:
1. **Explicit Range:** `bytes=1000-4999` (Specific slice).
2. **Open-Ended Prefix:** `bytes=5000-` (From byte 5000 to the EOF).
3. **Suffix Range:** `bytes=-1024` (The final 1024 bytes of the file, critical for reading MP4 `moov` metadata atoms located at the tail of a video).

### 4.2 Buffered Chunked I/O & Client Abandonment Detection
Video streams read directly from disk into a reusable **256 KB memory buffer** (`CHUNK_SIZE`):
```rust
let mut remaining = content_len;
let mut buffer = [0u8; CHUNK_SIZE];

while remaining > 0 {
    let to_read = (buffer.len() as u64).min(remaining) as usize;
    let bytes_read = file.read(&mut buffer[..to_read])?;
    if bytes_read == 0 { break; }
    
    if let Err(_) = stream.write_all(&buffer[..bytes_read]) {
        // Client scrubbed away or closed browser tab: cleanly abort loop
        break;
    }
    remaining -= bytes_read as u64;
}
```
If a client scrubs ahead on the timeline, the browser aborts the current TCP connection. Playbox detects the broken pipe on `stream.write_all()` and exits the loop immediately, reclaiming disk I/O and CPU without throwing an unhandled panic.

---

## 5. DLNA / UPnP Discovery via UDP Multicast (SSDP)

Playbox makes itself discoverable to smart TVs, game consoles, and VLC through the **Simple Service Discovery Protocol (SSDP)**:

1. **Multicast Listener (UDP 1900):**
   Joins the standard SSDP multicast group `239.255.255.250` on port `1900`.
2. **`M-SEARCH` Request Processing:**
   When a device on the network broadcasts an `M-SEARCH * HTTP/1.1` search for `urn:schemas-upnp-org:device:MediaServer:1` or `ssdp:all`, Playbox responds with unicast HTTP datagrams announcing its XML device description URL:
   ```http
   HTTP/1.1 200 OK
   CACHE-CONTROL: max-age=1800
   LOCATION: http://192.168.1.150:8080/description.xml
   SERVER: Playbox/1.0 UPnP/1.0
   ST: urn:schemas-upnp-org:device:MediaServer:1
   USN: uuid:playbox-192.168.1.150::urn:schemas-upnp-org:device:MediaServer:1
   ```
3. **SOAP / DIDL-Lite Metadata Generation:**
   Under `/ContentDirectory/Control`, Playbox responds to SOAP `Browse` actions by serializing the video library into DIDL-Lite XML metadata containers, enabling playback directly on smart TVs.

---

## 6. Algorithmic Engineering: QR Code Generator from Scratch

Rather than importing external image or QR libraries, Playbox implements an entire **ISO/IEC 18004 QR Code generator in ~600 lines of pure Rust `std`**.

### 6.1 Mathematical Principles
- **Galois Field $\text{GF}(2^8)$ Arithmetic:**
  Implements multiplication and division over polynomial field $x^8 + x^4 + x^3 + x^2 + 1$ (characteristic 0x11D / 285).
- **Reed-Solomon Error Correction:**
  Generates error-correcting polynomials with Level L (7% recovery) capability for QR Versions 1 through 6.
- **Mask Pattern Evaluation:**
  Evaluates all 8 standard QR mask patterns across four penalty rules (consecutive modules, 2x2 blocks, finder patterns, dark/light module ratio balance) to select the mathematically optimal mask.

### 6.2 Terminal Half-Block ANSI Rendering
Standard terminals have character cells that are roughly twice as tall as they are wide. Printing normal space or block characters results in vertically stretched, unscannable QR codes.

Playbox packs **two vertical QR modules into each single character cell** using Unicode half-block characters:
- ` ` (both light)
- `▀` (top dark, bottom light)
- `▄` (top light, bottom dark)
- `█` (both dark)

This halves the vertical line height, producing a square, high-contrast QR code that smartphone cameras can read directly off a CLI terminal:

```rust
match (top_module, bottom_module) {
    (true, true)   => print!("█"),
    (true, false)  => print!("▀"),
    (false, true)  => print!("▄"),
    (false, false) => print!(" "),
}
```

---

## 7. Subtitle Engine & On-The-Fly WebVTT Transcoding

HTML5 `<video>` tags only support WebVTT (`.vtt`) subtitles, whereas the vast majority of media libraries store subtitles in SubRip (`.srt`) format.

### 7.1 Filename Heuristic Scanner
Given `MyMovie.2024.1080p.mkv`, the scanner identifies:
- `MyMovie.2024.1080p.srt`
- `MyMovie.2024.1080p.en.srt`
- `MyMovie.2024.1080p_fa.vtt`
- `MyMovie.2024.1080p-es.srt`

### 7.2 Zero-Allocation String Transformation
When an `.srt` subtitle is requested via `/api/subtitles?path=...`, Playbox converts it on the fly:
1. Strips UTF-8 Byte Order Marks (BOM `\u{feff}`).
2. Inserts the mandatory `WEBVTT` signature.
3. Rewrites timestamp delimiters (`00:01:23,456 --> 00:01:25,789` $\to$ `00:01:23.456 --> 00:01:25.789`).
4. Serves with HTTP header `Content-Type: text/vtt; charset=utf-8`.

---

## 8. Security Architecture & Threat Mitigation

### 8.1 Path Traversal Attack Mitigation
Because the server accepts relative path parameters (e.g. `/api/stream?path=...`), it is vulnerable to directory traversal attacks (`../../etc/passwd` or encoded `%2e%2e%2f`).

Playbox enforces a dual-verification guard:
```rust
pub fn resolve_safe_path(root: &Path, rel_path: &str) -> Option<PathBuf> {
    let clean_rel = rel_path.trim_start_matches('/').replace('\\', "/");
    let joined = root.join(&clean_rel);

    // Guard 1: Syntactic prefix check
    if !joined.starts_with(root) {
        return None;
    }

    // Guard 2: Physical filesystem canonicalization (resolves symlinks)
    let canon_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if let Ok(canonical) = fs::canonicalize(&joined) {
        if canonical.starts_with(&canon_root) {
            return Some(canonical);
        }
    }
    None
}
```
If a symlink inside the folder points outside the designated root directory, `canonical.starts_with(&canon_root)` fails, returning `403 Forbidden`.

### 8.2 Safe JSON & XML Escaping
All filenames, sizes, and file paths are sanitized through manual ASCII control character and character entity encoders (`escape_json` and `escape_xml`), neutralizing XSS injection and XML entity exploits.

### 8.3 PIN Authentication Matrix
When `--pin <CODE>` is activated:
- Static assets and APIs return `401 Unauthorized` or render the PIN keypad.
- Authenticated state is validated through three channels:
  1. Cookie: `Cookie: pin=<CODE>; Path=/; SameSite=Lax`
  2. Custom Header: `X-Playbox-PIN: <CODE>`
  3. Query Parameter: `?pin=<CODE>`

---

## 9. Performance & Memory Profile Summary

| Metric | Measured Value | Implementation Driver |
| :--- | :--- | :--- |
| **Release Binary Size** | ~630 KB | 0 external crates, `strip = true` |
| **Idle RAM Usage** | < 8 MB | Fixed thread pool, no dynamic GC runtime |
| **Peak Streaming RAM** | ~4 MB per client | 256 KB bounded chunk buffer |
| **Disk I/O Overhead** | Minimal | Zero whole-file memory caching, native kernel range seek |
| **Cold Compile Time** | ~0.55s | Standard library only; no macro expansion crates |
