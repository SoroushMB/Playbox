// ==============================================================================
//  PLAYBOX - Minimalist LAN Video Streaming Server
//  100% Rust Standard Library (std) - Zero External Dependencies
// ==============================================================================

pub mod qr;
pub mod scanner;
pub mod thumbnail;
pub mod upnp;
pub mod streamer;
pub mod subtitles;
pub mod http;

use std::collections::HashSet;
use std::env;
use std::fs;
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use qr::print_qr_code;
use scanner::{scan_videos, VideoItem};
use thumbnail::pregenerate_thumbnails;
use upnp::ssdp_responder;
use http::handle_connection;

pub const DEFAULT_PORT: u16 = 8080;
pub const THUMBS_DIR_NAME: &str = ".playbox_thumbs";

#[derive(Clone)]
pub struct ServerConfig {
    pub root_dir: PathBuf,
    pub thumbs_dir: PathBuf,
    pub port: u16,
    pub local_ip: String,
    pub has_ffmpeg: bool,
    pub generating_thumbs: Arc<Mutex<HashSet<String>>>,
    pub videos: Arc<Mutex<Vec<VideoItem>>>,
    pub pin: Option<String>,
}

#[derive(Debug, PartialEq, Eq, Default)]
pub struct CliArgs {
    pub dir: Option<PathBuf>,
    pub port: Option<u16>,
    pub pin: Option<String>,
    pub show_help: bool,
}

pub fn parse_cli_args(args: &[String]) -> Result<CliArgs, String> {
    let mut parsed = CliArgs::default();
    let mut i = 1;

    while i < args.len() {
        let arg = &args[i];
        if arg == "-h" || arg == "--help" {
            parsed.show_help = true;
            return Ok(parsed);
        } else if arg == "--pin" {
            i += 1;
            if i >= args.len() {
                return Err("Missing code for --pin option".to_string());
            }
            parsed.pin = Some(args[i].clone());
        } else if let Some(code) = arg.strip_prefix("--pin=") {
            parsed.pin = Some(code.to_string());
        } else if arg == "-p" || arg == "--port" {
            i += 1;
            if i >= args.len() {
                return Err("Missing value for --port option".to_string());
            }
            let p: u16 = args[i].parse().map_err(|_| format!("Invalid port '{}'", args[i]))?;
            parsed.port = Some(p);
        } else if let Some(p_str) = arg.strip_prefix("--port=") {
            let p: u16 = p_str.parse().map_err(|_| format!("Invalid port '{}'", p_str))?;
            parsed.port = Some(p);
        } else if let Ok(p) = arg.parse::<u16>() {
            if parsed.port.is_none() {
                parsed.port = Some(p);
            } else if parsed.dir.is_none() {
                parsed.dir = Some(PathBuf::from(arg));
            }
        } else if parsed.dir.is_none() {
            parsed.dir = Some(PathBuf::from(arg));
        } else if parsed.port.is_none() {
            if let Ok(p) = arg.parse::<u16>() {
                parsed.port = Some(p);
            }
        }
        i += 1;
    }

    Ok(parsed)
}

fn print_help() {
    println!("PLAYBOX - Minimalist LAN Video Streaming Server (100% Rust std)");
    println!();
    println!("USAGE:");
    println!("  playbox                   Serve current directory on port 8080");
    println!("  playbox <PORT>            Serve current directory on specified port");
    println!("  playbox <DIR>             Serve specified directory on port 8080");
    println!("  playbox <DIR> <PORT>      Serve specified directory on specified port");
    println!();
    println!("OPTIONS:");
    println!("  --pin <CODE>              Require PIN code authentication to access server");
    println!("  -p, --port <PORT>         Specify port explicitly");
    println!("  -h, --help                Show this help message");
    println!();
}

pub fn get_local_ip() -> Option<String> {
    // UDP socket route inquiry - queries OS routing table without sending packets
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}

pub fn detect_firewall_hint(port: u16) -> Option<String> {
    // Check if ufw is active
    if let Ok(output) = Command::new("systemctl")
        .args(&["is-active", "ufw"])
        .output()
    {
        let status = String::from_utf8_lossy(&output.stdout);
        if status.trim() == "active" {
            return Some(format!("sudo ufw allow {}/tcp comment \"Playbox\"", port));
        }
    }
    // Check if firewalld is active
    if let Ok(output) = Command::new("systemctl")
        .args(&["is-active", "firewalld"])
        .output()
    {
        let status = String::from_utf8_lossy(&output.stdout);
        if status.trim() == "active" {
            return Some(format!(
                "sudo firewall-cmd --add-port={}/tcp --permanent && sudo firewall-cmd --reload",
                port
            ));
        }
    }
    None
}

fn main() {
    let raw_args: Vec<String> = env::args().collect();
    let cli_args = match parse_cli_args(&raw_args) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("[!] Argument error: {}", e);
            std::process::exit(1);
        }
    };

    if cli_args.show_help {
        print_help();
        return;
    }

    // Parse target directory
    let target_dir = cli_args.dir.unwrap_or_else(|| {
        env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    });

    let canonical_root = match fs::canonicalize(&target_dir) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[!] Failed to resolve directory {:?}: {}", target_dir, e);
            std::process::exit(1);
        }
    };

    // Set up thumbnails directory in the target folder
    let thumbs_dir = canonical_root.join(THUMBS_DIR_NAME);
    let _ = fs::create_dir_all(&thumbs_dir);

    // Check if ffmpeg is available
    let has_ffmpeg = Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);

    // Bind listener (with fallback to next open port if default is occupied)
    let (listener, active_port) = if let Some(p) = cli_args.port {
        let addr = format!("0.0.0.0:{}", p);
        match TcpListener::bind(&addr) {
            Ok(l) => (l, p),
            Err(e) => {
                eprintln!("[!] Could not bind to {}: {}", addr, e);
                std::process::exit(1);
            }
        }
    } else {
        let mut bound = None;
        for candidate in DEFAULT_PORT..(DEFAULT_PORT + 25) {
            let addr = format!("0.0.0.0:{}", candidate);
            if let Ok(l) = TcpListener::bind(&addr) {
                bound = Some((l, candidate));
                break;
            }
        }
        match bound {
            Some(pair) => pair,
            None => {
                eprintln!(
                    "[!] Could not find an open port from {} to {}",
                    DEFAULT_PORT,
                    DEFAULT_PORT + 25
                );
                std::process::exit(1);
            }
        }
    };

    let local_ip = get_local_ip().unwrap_or_else(|| "127.0.0.1".to_string());

    let config = ServerConfig {
        root_dir: canonical_root.clone(),
        thumbs_dir: thumbs_dir.clone(),
        port: active_port,
        local_ip: local_ip.clone(),
        has_ffmpeg,
        generating_thumbs: Arc::new(Mutex::new(HashSet::new())),
        videos: Arc::new(Mutex::new(Vec::new())),
        pin: cli_args.pin.clone(),
    };

    let initial_videos = scan_videos(&canonical_root);
    {
        let mut v = config.videos.lock().unwrap();
        *v = initial_videos.clone();
    }

    // Spawn background thread to pre-generate thumbnails for all scanned videos
    if has_ffmpeg {
        let cfg = config.clone();
        thread::spawn(move || {
            pregenerate_thumbnails(cfg);
        });
    }

    let network_url = format!("http://{}:{}", local_ip, active_port);
    let qr_url = match &cli_args.pin {
        Some(pin) => format!("http://{}:{}/?pin={}", local_ip, active_port, pin),
        None => network_url.clone(),
    };

    println!();
    println!("  ┌────────────────────────────────────────────────────────┐");
    println!("  │                     PLAYBOX SERVER                     │");
    println!("  │      Minimalist LAN Video Streaming (100% Rust std)    │");
    println!("  └────────────────────────────────────────────────────────┘");
    println!();
    println!("  [●] Local:    http://localhost:{}", active_port);
    println!("  [●] Network:  {}", network_url);
    println!("  [●] Root:     {}", canonical_root.display());
    println!("  [●] Videos:   {} found", initial_videos.len());
    println!(
        "  [●] Thumbs:   {}",
        if has_ffmpeg {
            "FFmpeg active (Hardware/Server)"
        } else {
            "Client canvas fallback"
        }
    );
    if let Some(pin) = &cli_args.pin {
        println!("  [●] Security: PIN Protected ({})", pin);
    }
    println!();

    // Firewall hint: check if ufw or firewalld is active
    let fw_hint = detect_firewall_hint(active_port);
    if let Some(hint) = fw_hint {
        println!("  ⚠  FIREWALL DETECTED — to allow LAN access, run:");
        println!("     {}", hint);
        println!();
    }

    // Print QR code for the network URL
    println!("  Scan to connect from your phone:");
    println!();
    print_qr_code(&qr_url);
    println!();
    println!("  Press Ctrl+C to terminate the server.");
    println!();

    // Spawn SSDP responder thread for UPnP discovery
    {
        let cfg = config.clone();
        thread::spawn(move || {
            ssdp_responder(cfg);
        });
    }

    // Fixed-size thread pool (16 workers) consuming jobs from an mpsc channel
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
                    Err(_) => break,
                }
            }
        });
    }

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(60)));
                let cfg = config.clone();
                if tx.send((stream, cfg)).is_err() {
                    eprintln!("[!] Thread pool channel closed");
                    break;
                }
            }
            Err(e) => eprintln!("[!] Incoming connection error: {}", e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cli_args_defaults() {
        let args = vec!["playbox".to_string()];
        let parsed = parse_cli_args(&args).unwrap();
        assert_eq!(parsed.dir, None);
        assert_eq!(parsed.port, None);
        assert_eq!(parsed.pin, None);
        assert!(!parsed.show_help);
    }

    #[test]
    fn test_parse_cli_args_port_only() {
        let args = vec!["playbox".to_string(), "9000".to_string()];
        let parsed = parse_cli_args(&args).unwrap();
        assert_eq!(parsed.port, Some(9000));
        assert_eq!(parsed.dir, None);
    }

    #[test]
    fn test_parse_cli_args_dir_only() {
        let args = vec!["playbox".to_string(), "/tmp/videos".to_string()];
        let parsed = parse_cli_args(&args).unwrap();
        assert_eq!(parsed.dir, Some(PathBuf::from("/tmp/videos")));
        assert_eq!(parsed.port, None);
    }

    #[test]
    fn test_parse_cli_args_dir_and_port() {
        let args = vec!["playbox".to_string(), "/tmp/videos".to_string(), "9000".to_string()];
        let parsed = parse_cli_args(&args).unwrap();
        assert_eq!(parsed.dir, Some(PathBuf::from("/tmp/videos")));
        assert_eq!(parsed.port, Some(9000));
    }

    #[test]
    fn test_parse_cli_args_pin() {
        let args = vec!["playbox".to_string(), "--pin".to_string(), "4321".to_string()];
        let parsed = parse_cli_args(&args).unwrap();
        assert_eq!(parsed.pin, Some("4321".to_string()));

        let args2 = vec!["playbox".to_string(), "--pin=1234".to_string(), "8080".to_string()];
        let parsed2 = parse_cli_args(&args2).unwrap();
        assert_eq!(parsed2.pin, Some("1234".to_string()));
        assert_eq!(parsed2.port, Some(8080));
    }

    #[test]
    fn test_parse_cli_args_help() {
        let args = vec!["playbox".to_string(), "--help".to_string()];
        let parsed = parse_cli_args(&args).unwrap();
        assert!(parsed.show_help);
    }
}
