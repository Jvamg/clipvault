use std::borrow::Cow;
use std::io::{Cursor, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use arboard::{Clipboard, ImageData};
use chrono::Utc;
use image::{DynamicImage, ImageFormat, RgbaImage};

use crate::domain::ClipEntry;
use crate::storage::Storage;

/// Default clipboard polling interval without excessive busy-waiting.
pub const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Interval between periodic sweeps purging expired secrets in the daemon.
pub const GC_INTERVAL: Duration = Duration::from_secs(5);

/// Captured clipboard content (plain text or PNG-compressed image).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardPayload {
    Text(String),
    Image {
        png_bytes: Vec<u8>,
        width: u32,
        height: u32,
    },
}

/// Raw event captured by the watcher and sent to the consumer for persistence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardEvent {
    pub payload: ClipboardPayload,
    pub timestamp: i64,
}

/// Encodes a raw RGBA pixel buffer into compressed PNG bytes.
pub fn encode_rgba_to_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    let img_buf = RgbaImage::from_raw(width, height, rgba.to_vec())
        .context("invalid RGBA dimensions for PNG encoding")?;
    let mut png_bytes = Vec::new();
    DynamicImage::ImageRgba8(img_buf)
        .write_to(&mut Cursor::new(&mut png_bytes), ImageFormat::Png)
        .context("failed to encode RGBA image to PNG")?;
    Ok(png_bytes)
}

/// Reads the `(width, height)` dimensions of an encoded image (e.g., PNG).
pub fn decode_png_dimensions(png_bytes: &[u8]) -> Result<(u32, u32)> {
    let img =
        image::load_from_memory(png_bytes).context("failed to decode PNG image metadata")?;
    Ok((img.width(), img.height()))
}

/// Decodes PNG bytes into `(width, height, RGBA pixels)`.
pub fn decode_png_to_rgba(png_bytes: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let img =
        image::load_from_memory(png_bytes).context("failed to decode PNG image to RGBA")?;
    let width = img.width();
    let height = img.height();
    let rgba = img.to_rgba8().into_raw();
    Ok((width, height, rgba))
}

/// Decides whether the current clipboard content should be ignored by the watcher.
///
/// Returns `true` when:
/// - the hash matches `last_seen_hash` (no actual change since the last poll); or
/// - the hash matches `ignored_hash` (change triggered by `clipvault` itself when restoring an item).
pub fn should_ignore_clip(
    current_hash: &str,
    last_seen_hash: &mut Option<String>,
    ignored_hash: &mut Option<String>,
) -> bool {
    if last_seen_hash.as_deref() == Some(current_hash) {
        if ignored_hash.as_deref() == Some(current_hash) {
            *ignored_hash = None;
        }
        return true;
    }

    *last_seen_hash = Some(current_hash.to_string());

    if ignored_hash.as_deref() == Some(current_hash) {
        *ignored_hash = None;
        return true;
    }

    false
}

/// Attempts to read clipboard text using system CLI tools when `arboard` fails or returns empty.
pub fn read_clipboard_fallback() -> Option<String> {
    const CANDIDATES: &[(&str, &[&str])] = &[
        ("xclip", &["-o", "-selection", "clipboard"]),
        ("xsel", &["-ob"]),
        ("wl-paste", &["--no-newline"]),
    ];

    for (bin, args) in CANDIDATES {
        let output = match Command::new(bin)
            .args(*args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        {
            Ok(out) if out.status.success() => out,
            _ => continue,
        };

        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        if !text.trim().is_empty() {
            return Some(text);
        }
    }

    None
}

/// Attempts to read an `image/png` clipboard payload via Wayland (`wl-paste`) or X11 (`xclip`).
pub fn read_clipboard_image_fallback() -> Option<(Vec<u8>, u32, u32)> {
    // 1. Wayland: wl-paste --list-types -> wl-paste --type image/png
    if let Ok(types_out) = Command::new("wl-paste")
        .arg("--list-types")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    {
        if types_out.status.success() {
            let types = String::from_utf8_lossy(&types_out.stdout);
            if types
                .lines()
                .any(|line| line.trim().eq_ignore_ascii_case("image/png"))
            {
                if let Ok(img_out) = Command::new("wl-paste")
                    .args(["--type", "image/png"])
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .output()
                {
                    if img_out.status.success() && !img_out.stdout.is_empty() {
                        if let Ok((width, height)) = decode_png_dimensions(&img_out.stdout) {
                            if width > 0 && height > 0 {
                                return Some((img_out.stdout, width, height));
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. X11: xclip -selection clipboard -t TARGETS -o -> -t image/png -o
    if let Ok(targets_out) = Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "TARGETS", "-o"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    {
        if targets_out.status.success() {
            let targets = String::from_utf8_lossy(&targets_out.stdout);
            if targets
                .lines()
                .any(|line| line.trim().eq_ignore_ascii_case("image/png"))
            {
                if let Ok(img_out) = Command::new("xclip")
                    .args(["-selection", "clipboard", "-t", "image/png", "-o"])
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .output()
                {
                    if img_out.status.success() && !img_out.stdout.is_empty() {
                        if let Ok((width, height)) = decode_png_dimensions(&img_out.stdout) {
                            if width > 0 && height > 0 {
                                return Some((img_out.stdout, width, height));
                            }
                        }
                    }
                }
            }
        }
    }

    None
}

enum ArboardReadResult {
    Payload(ClipboardPayload),
    SameCachedImage,
    EmptyOrError,
}

fn read_arboard_payload(
    clipboard: &mut Option<Clipboard>,
    last_arboard_err: &mut Option<String>,
    raw_rgba_cache: &mut Option<(String, String)>,
    last_seen_hash: &Option<String>,
) -> ArboardReadResult {
    let Some(cb) = clipboard.as_mut() else {
        return ArboardReadResult::EmptyOrError;
    };

    if let Ok(img) = cb.get_image() {
        if img.width > 0 && img.height > 0 {
            let width = img.width as u32;
            let height = img.height as u32;
            let raw_key = format!(
                "{width}x{height}:{}",
                ClipEntry::compute_bytes_hash(img.bytes.as_ref())
            );

            if let Some((cached_raw, cached_png_hash)) = raw_rgba_cache.as_ref() {
                if cached_raw == &raw_key && last_seen_hash.as_deref() == Some(cached_png_hash.as_str()) {
                    *last_arboard_err = None;
                    return ArboardReadResult::SameCachedImage;
                }
            }

            match encode_rgba_to_png(width, height, img.bytes.as_ref()) {
                Ok(png_bytes) => {
                    let png_hash = ClipEntry::compute_bytes_hash(&png_bytes);
                    *raw_rgba_cache = Some((raw_key, png_hash));
                    *last_arboard_err = None;
                    return ArboardReadResult::Payload(ClipboardPayload::Image {
                        png_bytes,
                        width,
                        height,
                    });
                }
                Err(err) => {
                    eprintln!("[clipvault-watcher] failed to encode clipboard image: {err:#}");
                }
            }
        }
    } else {
        *raw_rgba_cache = None;
    }

    match cb.get_text() {
        Ok(text) if !text.trim().is_empty() => {
            *last_arboard_err = None;
            return ArboardReadResult::Payload(ClipboardPayload::Text(text));
        }
        Ok(_) | Err(arboard::Error::ContentNotAvailable) => {}
        Err(err) => {
            let msg = err.to_string();
            if last_arboard_err.as_deref() != Some(msg.as_str()) {
                eprintln!("[clipvault-watcher] arboard read error: {msg}");
                *last_arboard_err = Some(msg);
            }
        }
    }

    ArboardReadResult::EmptyOrError
}

/// Returns true if the current session is running under Wayland.
pub fn is_wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
        || std::env::var("XDG_SESSION_TYPE")
            .map(|v| v.eq_ignore_ascii_case("wayland"))
            .unwrap_or(false)
}

/// Copies text to the system clipboard.
/// On Wayland (or when CLI helpers are available), prioritizes `wl-copy` / `xclip` / `xsel`,
/// which fork to the background and keep content alive after `clipvault ui` exits.
pub fn set_clipboard_text(content: &str) -> Result<()> {
    let wayland_first: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-i", "-selection", "clipboard"]),
        ("xsel", &["-ib"]),
    ];
    let x11_first: &[(&str, &[&str])] = &[
        ("xclip", &["-i", "-selection", "clipboard"]),
        ("xsel", &["-ib"]),
        ("wl-copy", &[]),
    ];
    let writers = if is_wayland_session() {
        wayland_first
    } else {
        x11_first
    };

    if write_stdin_to_first_cli(writers, content.as_bytes()) {
        return Ok(());
    }

    match Clipboard::new().and_then(|mut cb| cb.set_text(content.to_string())) {
        Ok(()) => {
            std::thread::sleep(Duration::from_millis(50));
            Ok(())
        }
        Err(err) => {
            anyhow::bail!(
                "failed to copy to clipboard via CLI (wl-copy/xclip/xsel) and arboard ({err})"
            )
        }
    }
}

/// Restores PNG image bytes to the system clipboard.
/// Prioritizes `wl-copy --type image/png` on Wayland (and `xclip` on X11) so the image
/// persists after the TUI exits, falling back to `arboard`.
pub fn set_clipboard_image(png_bytes: &[u8]) -> Result<()> {
    let wayland_first: &[(&str, &[&str])] = &[
        ("wl-copy", &["--type", "image/png"]),
        ("xclip", &["-selection", "clipboard", "-t", "image/png", "-i"]),
    ];
    let x11_first: &[(&str, &[&str])] = &[
        ("xclip", &["-selection", "clipboard", "-t", "image/png", "-i"]),
        ("wl-copy", &["--type", "image/png"]),
    ];
    let writers = if is_wayland_session() {
        wayland_first
    } else {
        x11_first
    };

    if write_stdin_to_first_cli(writers, png_bytes) {
        return Ok(());
    }

    if let Ok((width, height, rgba)) = decode_png_to_rgba(png_bytes) {
        let image_data = ImageData {
            width: width as usize,
            height: height as usize,
            bytes: Cow::Owned(rgba),
        };
        if let Ok(()) = Clipboard::new().and_then(|mut cb| cb.set_image(image_data)) {
            std::thread::sleep(Duration::from_millis(50));
            return Ok(());
        }
    }

    anyhow::bail!(
        "failed to restore image to clipboard via CLI (wl-copy/xclip) and arboard"
    )
}

fn write_stdin_to_first_cli(writers: &[(&str, &[&str])], bytes: &[u8]) -> bool {
    for (bin, args) in writers {
        let mut child = match Command::new(bin)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => continue,
        };

        if let Some(mut stdin) = child.stdin.take() {
            if stdin.write_all(bytes).is_err() || stdin.flush().is_err() {
                drop(stdin);
                let _ = child.wait();
                continue;
            }
            drop(stdin);
        }

        if let Ok(status) = child.wait() {
            if status.success() {
                return true;
            }
        }
    }

    false
}

/// Background clipboard monitor using `arboard` (with CLI fallback) and an `mpsc` channel.
pub struct ClipboardWatcher {
    storage: Arc<Storage>,
    ignored_hash: Arc<Mutex<Option<String>>>,
    poll_interval: Duration,
}

impl ClipboardWatcher {
    pub fn new(storage: Arc<Storage>) -> Self {
        Self {
            storage,
            ignored_hash: Arc::new(Mutex::new(None)),
            poll_interval: POLL_INTERVAL,
        }
    }

    /// Returns a shared handle for registering hashes that should be ignored.
    #[allow(dead_code)]
    pub fn ignored_handle(&self) -> Arc<Mutex<Option<String>>> {
        Arc::clone(&self.ignored_hash)
    }

    /// Records a restored text string in memory so its clipboard echo is ignored.
    #[allow(dead_code)]
    pub fn mark_ignored(&self, content: &str) {
        let hash = ClipEntry::compute_hash(content);
        if let Ok(mut guard) = self.ignored_hash.lock() {
            *guard = Some(hash);
        }
    }

    /// Spawns the background watcher thread and returns the captured event receiver.
    pub fn spawn(
        self,
        stop_flag: Arc<AtomicBool>,
    ) -> Result<(mpsc::Receiver<ClipboardEvent>, JoinHandle<Result<()>>)> {
        let (tx, rx) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("clipvault-watcher".to_string())
            .spawn(move || self.run_loop(tx, stop_flag))
            .context("failed to spawn watcher thread")?;

        Ok((rx, handle))
    }

    fn run_loop(
        self,
        tx: mpsc::Sender<ClipboardEvent>,
        stop_flag: Arc<AtomicBool>,
    ) -> Result<()> {
        // Instantiate once and keep the handle alive for the entire watcher lifetime.
        let mut clipboard = match Clipboard::new() {
            Ok(cb) => Some(cb),
            Err(err) => {
                eprintln!(
                    "[clipvault-watcher] warning: failed to initialize arboard ({err}); falling back to CLI (wl-paste/xclip/xsel)."
                );
                None
            }
        };

        let mut last_seen_hash: Option<String> = None;
        let mut last_arboard_err: Option<String> = None;
        let mut raw_rgba_cache: Option<(String, String)> = None;
        let mut last_gc = Instant::now();

        if let Err(err) = self.storage.purge_expired(Utc::now().timestamp()) {
            eprintln!("[clipvault-watcher] error during initial expired items cleanup: {err:#}");
        }

        while !stop_flag.load(Ordering::Relaxed) {
            let now_ts = Utc::now().timestamp();

            if last_gc.elapsed() >= GC_INTERVAL {
                if let Err(err) = self.storage.purge_expired(now_ts) {
                    eprintln!("[clipvault-watcher] error during expired items GC: {err:#}");
                }
                last_gc = Instant::now();
            }

            match self.storage.consume_last_restored_hash() {
                Ok(Some(restored_hash)) => {
                    if let Ok(mut guard) = self.ignored_hash.lock() {
                        *guard = Some(restored_hash);
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    eprintln!(
                        "[clipvault-watcher] error reading restored hash from SQLite: {err:#}"
                    );
                }
            }

            let payload_opt = match read_arboard_payload(
                &mut clipboard,
                &mut last_arboard_err,
                &mut raw_rgba_cache,
                &last_seen_hash,
            ) {
                ArboardReadResult::Payload(payload) => Some(payload),
                ArboardReadResult::SameCachedImage => {
                    if let Ok(mut guard) = self.ignored_hash.lock() {
                        if guard.as_deref() == last_seen_hash.as_deref() {
                            *guard = None;
                        }
                    }
                    None
                }
                ArboardReadResult::EmptyOrError => {
                    if let Some((png_bytes, width, height)) = read_clipboard_image_fallback() {
                        Some(ClipboardPayload::Image {
                            png_bytes,
                            width,
                            height,
                        })
                    } else {
                        read_clipboard_fallback().map(ClipboardPayload::Text)
                    }
                }
            };

            if let Some(payload) = payload_opt {
                let current_hash = match &payload {
                    ClipboardPayload::Text(text) => ClipEntry::compute_hash(text),
                    ClipboardPayload::Image { png_bytes, .. } => {
                        ClipEntry::compute_bytes_hash(png_bytes)
                    }
                };

                let ignore = {
                    let mut guard = self.ignored_hash.lock().unwrap_or_else(|e| e.into_inner());
                    should_ignore_clip(&current_hash, &mut last_seen_hash, &mut guard)
                };

                if !ignore {
                    let event = ClipboardEvent {
                        payload,
                        timestamp: now_ts,
                    };
                    if tx.send(event).is_err() {
                        break;
                    }
                }
            }

            thread::sleep(self.poll_interval);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_new_clip_and_ignores_unchanged_polls() {
        let mut last_seen = None;
        let mut ignored = None;
        let h1 = ClipEntry::compute_hash("first text");

        assert!(!should_ignore_clip(&h1, &mut last_seen, &mut ignored));
        assert_eq!(last_seen.as_deref(), Some(h1.as_str()));

        // Repeated polls with identical content should be ignored
        assert!(should_ignore_clip(&h1, &mut last_seen, &mut ignored));
        assert!(should_ignore_clip(&h1, &mut last_seen, &mut ignored));
    }

    #[test]
    fn suppresses_self_restored_clip_once_then_allows_future_manual_copies() {
        let mut last_seen = Some(ClipEntry::compute_hash("current text"));
        let h_restored = ClipEntry::compute_hash("item restored by clipvault");
        let mut ignored = Some(h_restored.clone());

        // Restore echo should be ignored once and clear `ignored`
        assert!(should_ignore_clip(&h_restored, &mut last_seen, &mut ignored));
        assert!(ignored.is_none());
        assert_eq!(last_seen.as_deref(), Some(h_restored.as_str()));

        // User copies something else
        let h_other = ClipEntry::compute_hash("other content");
        assert!(!should_ignore_clip(&h_other, &mut last_seen, &mut ignored));

        // User manually copies the same entry again -> should now be captured
        assert!(!should_ignore_clip(
            &h_restored,
            &mut last_seen,
            &mut ignored
        ));
    }

    #[test]
    fn restoring_already_active_clip_clears_ignored_hash() {
        let h = ClipEntry::compute_hash("same item");
        let mut last_seen = Some(h.clone());
        let mut ignored = Some(h.clone());

        assert!(should_ignore_clip(&h, &mut last_seen, &mut ignored));
        assert!(ignored.is_none());
    }

    #[test]
    fn mark_ignored_populates_shared_handle_with_sha256() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Arc::new(Storage::open(dir.path().join("watcher_test.db")).expect("open"));
        let watcher = ClipboardWatcher::new(storage);
        let handle = watcher.ignored_handle();

        watcher.mark_ignored("restored text");
        let recorded = handle.lock().expect("lock").clone();
        assert_eq!(recorded, Some(ClipEntry::compute_hash("restored text")));
    }

    #[test]
    fn png_roundtrip_encoding_and_decoding_preserve_dimensions_and_pixels() {
        let rgba: Vec<u8> = vec![
            255, 0, 0, 255, // red
            0, 255, 0, 255, // green
            0, 0, 255, 255, // blue
            255, 255, 255, 255, // white
        ];
        let png = encode_rgba_to_png(2, 2, &rgba).expect("encode png");
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));

        let (w, h) = decode_png_dimensions(&png).expect("decode dims");
        assert_eq!((w, h), (2, 2));

        let (dw, dh, decoded_rgba) = decode_png_to_rgba(&png).expect("decode rgba");
        assert_eq!((dw, dh), (2, 2));
        assert_eq!(decoded_rgba, rgba);
    }

    #[test]
    fn mpsc_clipboard_events_persist_text_and_image_into_storage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Arc::new(Storage::open(dir.path().join("mpsc_test.db")).expect("open"));
        let (tx, rx) = mpsc::channel();

        let text_event = ClipboardEvent {
            payload: ClipboardPayload::Text(r#"{"service":"clipvault","ok":true}"#.to_string()),
            timestamp: 1_700_000_000,
        };
        let png = encode_rgba_to_png(2, 2, &[10; 16]).expect("png");
        let img_event = ClipboardEvent {
            payload: ClipboardPayload::Image {
                png_bytes: png.clone(),
                width: 2,
                height: 2,
            },
            timestamp: 1_700_000_005,
        };

        tx.send(text_event).expect("send text");
        tx.send(img_event).expect("send image");
        drop(tx);

        for received in rx {
            match received.payload {
                ClipboardPayload::Text(text) => {
                    storage
                        .insert_or_update(&text, received.timestamp)
                        .expect("insert text");
                }
                ClipboardPayload::Image {
                    png_bytes,
                    width,
                    height,
                } => {
                    storage
                        .insert_or_update_image(png_bytes, width, height, received.timestamp)
                        .expect("insert image");
                }
            }
        }

        let listed = storage.list_all(1_700_000_010).expect("list_all");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].kind, crate::domain::ContentKind::Image);
        assert_eq!(listed[0].image_blob.as_deref(), Some(png.as_slice()));
        assert_eq!(listed[1].kind, crate::domain::ContentKind::Json);
    }
}
