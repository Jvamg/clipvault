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

/// Monitor de clipboard em segundo plano utilizando `arboard` e canal `mpsc`.
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

    /// Inicia a thread de monitoramento em segundo plano e retorna o receptor de novos itens.
    pub fn spawn(
        self,
        stop_flag: Arc<AtomicBool>,
    ) -> Result<(mpsc::Receiver<ClipEntry>, JoinHandle<Result<()>>)> {
        let (tx, rx) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("clipvault-watcher".to_string())
            .spawn(move || self.run_loop(tx, stop_flag))
            .context("falha ao iniciar thread do watcher")?;

        Ok((rx, handle))
    }

    fn run_loop(self, tx: mpsc::Sender<ClipEntry>, stop_flag: Arc<AtomicBool>) -> Result<()> {
        let mut clipboard = Clipboard::new()
            .context("falha ao inicializar acesso ao clipboard X11/sistema via arboard")?;
        let mut last_seen_hash: Option<String> = None;
        let mut last_gc = Instant::now();

        let _ = self.storage.purge_expired(Utc::now().timestamp());

        while !stop_flag.load(Ordering::Relaxed) {
            let now_ts = Utc::now().timestamp();

            if last_gc.elapsed() >= GC_INTERVAL {
                let _ = self.storage.purge_expired(now_ts);
                last_gc = Instant::now();
            }

            if let Ok(Some(restored_hash)) = self.storage.consume_last_restored_hash() {
                if let Ok(mut guard) = self.ignored_hash.lock() {
                    *guard = Some(restored_hash);
                }
            }

            match clipboard.get_text() {
                Ok(text) if !text.trim().is_empty() => {
                    let current_hash = ClipEntry::compute_hash(&text);
                    let ignore = {
                        let mut guard = self.ignored_hash.lock().unwrap_or_else(|e| e.into_inner());
                        should_ignore_clip(&current_hash, &mut last_seen_hash, &mut guard)
                    };

                    if !ignore {
                        if let Ok(Some(entry)) = self.storage.insert_or_update(&text, now_ts) {
                            if tx.send(entry).is_err() {
                                break;
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(arboard::Error::ContentNotAvailable) => {}
                Err(err) => {
                    // Tenta reinicializar a conexão caso o clipboard X11 tenha sido reiniciado.
                    if let Ok(reconnected) = Clipboard::new() {
                        clipboard = reconnected;
                    } else {
                        eprintln!("[clipvault-watcher] aviso ao ler clipboard: {err}");
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
        let storage = Arc::new(Storage::open(dir.path().join("watcher_test.redb")).expect("open"));
        let watcher = ClipboardWatcher::new(storage);
        let handle = watcher.ignored_handle();

        watcher.mark_ignored("texto restaurado");
        let recorded = handle.lock().expect("lock").clone();
        assert_eq!(recorded, Some(ClipEntry::compute_hash("texto restaurado")));
    }
}
