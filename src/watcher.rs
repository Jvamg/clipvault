use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use arboard::Clipboard;
use chrono::Utc;

use crate::domain::ClipEntry;
use crate::storage::Storage;

/// Intervalo padrão de polling do clipboard sem busy-waiting excessivo.
pub const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Intervalo entre varreduras periódicas de limpeza de segredos expirados no daemon.
pub const GC_INTERVAL: Duration = Duration::from_secs(5);

/// Evento bruto capturado pelo watcher e enviado ao consumidor para persistência.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardEvent {
    pub content: String,
    pub timestamp: i64,
}

/// Decide se o conteúdo atual lido do clipboard deve ser ignorado pelo watcher.
///
/// Retorna `true` quando:
/// - o hash coincide com `last_seen_hash` (sem alteração real desde o último poll); ou
/// - o hash coincide com `ignored_hash` (alteração originada pelo próprio `clipvault` ao restaurar um item).
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

/// Tenta ler o clipboard usando ferramentas CLI do sistema quando `arboard` falha ou retorna vazio.
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

/// Lê o texto atual do clipboard utilizando a instância persistente do `arboard`
/// com fallback automático para `xclip`/`xsel`/`wl-paste`.
fn read_clipboard_text(
    clipboard: &mut Option<Clipboard>,
    last_arboard_err: &mut Option<String>,
) -> Option<String> {
    if let Some(cb) = clipboard.as_mut() {
        match cb.get_text() {
            Ok(text) if !text.trim().is_empty() => {
                *last_arboard_err = None;
                return Some(text);
            }
            Ok(_) | Err(arboard::Error::ContentNotAvailable) => {}
            Err(err) => {
                let msg = err.to_string();
                if last_arboard_err.as_deref() != Some(msg.as_str()) {
                    eprintln!("[clipvault-watcher] erro de leitura no arboard: {msg}");
                    *last_arboard_err = Some(msg);
                }
            }
        }
    }

    read_clipboard_fallback()
}

/// Copia texto para o clipboard do sistema via `arboard`, com fallback CLI (`xclip`/`xsel`/`wl-copy`).
pub fn set_clipboard_text(content: &str) -> Result<()> {
    match Clipboard::new().and_then(|mut cb| cb.set_text(content.to_string())) {
        Ok(()) => {
            // Breve janela para o gerenciador de seleção X11 capturar o conteúdo antes do exit.
            std::thread::sleep(Duration::from_millis(50));
            return Ok(());
        }
        Err(err) => {
            eprintln!("[clipvault] aviso ao escrever via arboard ({err}); tentando fallback CLI.");
        }
    }

    const WRITERS: &[(&str, &[&str])] = &[
        ("xclip", &["-i", "-selection", "clipboard"]),
        ("xsel", &["-ib"]),
        ("wl-copy", &[]),
    ];

    for (bin, args) in WRITERS {
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
            if stdin.write_all(content.as_bytes()).is_err() {
                let _ = child.wait();
                continue;
            }
        }

        if let Ok(status) = child.wait() {
            if status.success() {
                return Ok(());
            }
        }
    }

    anyhow::bail!(
        "falha ao copiar para o clipboard via arboard e nenhum fallback CLI (xclip/xsel/wl-copy) funcionou"
    )
}

/// Monitor de clipboard em segundo plano utilizando `arboard` (com fallback CLI) e canal `mpsc`.
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

    /// Retorna um handle compartilhado para registrar hashes que devem ser ignorados.
    #[allow(dead_code)]
    pub fn ignored_handle(&self) -> Arc<Mutex<Option<String>>> {
        Arc::clone(&self.ignored_hash)
    }

    /// Registra em memória um texto restaurado pelo próprio `clipvault` para evitar eco.
    #[allow(dead_code)]
    pub fn mark_ignored(&self, content: &str) {
        let hash = ClipEntry::compute_hash(content);
        if let Ok(mut guard) = self.ignored_hash.lock() {
            *guard = Some(hash);
        }
    }

    /// Inicia a thread de monitoramento em segundo plano e retorna o receptor de eventos capturados.
    pub fn spawn(
        self,
        stop_flag: Arc<AtomicBool>,
    ) -> Result<(mpsc::Receiver<ClipboardEvent>, JoinHandle<Result<()>>)> {
        let (tx, rx) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("clipvault-watcher".to_string())
            .spawn(move || self.run_loop(tx, stop_flag))
            .context("falha ao iniciar thread do watcher")?;

        Ok((rx, handle))
    }

    fn run_loop(
        self,
        tx: mpsc::Sender<ClipboardEvent>,
        stop_flag: Arc<AtomicBool>,
    ) -> Result<()> {
        // Instancia uma única vez e mantém o objeto vivo durante toda a execução no X11.
        let mut clipboard = match Clipboard::new() {
            Ok(cb) => Some(cb),
            Err(err) => {
                eprintln!(
                    "[clipvault-watcher] aviso: falha ao inicializar arboard ({err}); usando fallback CLI (xclip/xsel/wl-paste)."
                );
                None
            }
        };

        let mut last_seen_hash: Option<String> = None;
        let mut last_arboard_err: Option<String> = None;
        let mut last_gc = Instant::now();

        if let Err(err) = self.storage.purge_expired(Utc::now().timestamp()) {
            eprintln!("[clipvault-watcher] erro na limpeza inicial de expirados: {err:#}");
        }

        while !stop_flag.load(Ordering::Relaxed) {
            let now_ts = Utc::now().timestamp();

            if last_gc.elapsed() >= GC_INTERVAL {
                if let Err(err) = self.storage.purge_expired(now_ts) {
                    eprintln!("[clipvault-watcher] erro no GC de expirados: {err:#}");
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
                        "[clipvault-watcher] erro ao ler hash restaurado no SQLite: {err:#}"
                    );
                }
            }

            if let Some(text) = read_clipboard_text(&mut clipboard, &mut last_arboard_err) {
                let current_hash = ClipEntry::compute_hash(&text);
                let ignore = {
                    let mut guard = self.ignored_hash.lock().unwrap_or_else(|e| e.into_inner());
                    should_ignore_clip(&current_hash, &mut last_seen_hash, &mut guard)
                };

                if !ignore {
                    let event = ClipboardEvent {
                        content: text,
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
        let h1 = ClipEntry::compute_hash("primeiro texto");

        assert!(!should_ignore_clip(&h1, &mut last_seen, &mut ignored));
        assert_eq!(last_seen.as_deref(), Some(h1.as_str()));

        // Polls repetidos com o mesmo conteúdo devem ser ignorados
        assert!(should_ignore_clip(&h1, &mut last_seen, &mut ignored));
        assert!(should_ignore_clip(&h1, &mut last_seen, &mut ignored));
    }

    #[test]
    fn suppresses_self_restored_clip_once_then_allows_future_manual_copies() {
        let mut last_seen = Some(ClipEntry::compute_hash("texto atual"));
        let h_restored = ClipEntry::compute_hash("item restaurado pelo clipvault");
        let mut ignored = Some(h_restored.clone());

        // Eco da restauração deve ser ignorado e limpar `ignored`
        assert!(should_ignore_clip(&h_restored, &mut last_seen, &mut ignored));
        assert!(ignored.is_none());
        assert_eq!(last_seen.as_deref(), Some(h_restored.as_str()));

        // Usuário copia outro conteúdo
        let h_other = ClipEntry::compute_hash("outro conteudo");
        assert!(!should_ignore_clip(&h_other, &mut last_seen, &mut ignored));

        // Usuário copia manualmente o mesmo item novamente -> agora deve ser capturado
        assert!(!should_ignore_clip(
            &h_restored,
            &mut last_seen,
            &mut ignored
        ));
    }

    #[test]
    fn restoring_already_active_clip_clears_ignored_hash() {
        let h = ClipEntry::compute_hash("mesmo item");
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

        watcher.mark_ignored("texto restaurado");
        let recorded = handle.lock().expect("lock").clone();
        assert_eq!(recorded, Some(ClipEntry::compute_hash("texto restaurado")));
    }

    #[test]
    fn mpsc_clipboard_event_persists_into_storage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Arc::new(Storage::open(dir.path().join("mpsc_test.db")).expect("open"));
        let (tx, rx) = mpsc::channel();

        let event = ClipboardEvent {
            content: r#"{"service":"clipvault","ok":true}"#.to_string(),
            timestamp: 1_700_000_000,
        };
        tx.send(event).expect("send event");
        drop(tx);

        let received = rx.recv().expect("receive event");
        let saved = storage
            .insert_or_update(&received.content, received.timestamp)
            .expect("insert_or_update")
            .expect("entry created");

        assert_eq!(saved.kind, crate::domain::ContentKind::Json);
        let listed = storage.list_all(1_700_000_005).expect("list_all");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, saved.id);
    }
}
