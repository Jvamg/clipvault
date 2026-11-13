mod classifier;
mod domain;
mod storage;
mod tui;
mod watcher;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Utc;
use clap::{Parser, Subcommand};

use crate::storage::Storage;
use crate::watcher::{ClipboardWatcher, GC_INTERVAL};

#[derive(Parser, Debug)]
#[command(
    name = "clipvault",
    version,
    about = "Gerenciador e Time-Machine inteligente de clipboard para Linux (X11)"
)]
struct Cli {
    /// Caminho alternativo para o arquivo do banco local redb
    #[arg(long, global = true, value_name = "PATH")]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Executa o monitor de clipboard e o coletor de segredos expirados em segundo plano
    Daemon,
    /// Abre a interface interativa de terminal (padrão quando nenhum subcomando é informado)
    Ui,
}

fn run_daemon(storage: Arc<Storage>) -> Result<()> {
    let stop_flag = Arc::new(AtomicBool::new(false));
    let watcher = ClipboardWatcher::new(Arc::clone(&storage));
    let (rx, handle) = watcher.spawn(Arc::clone(&stop_flag))?;

    println!(
        "[clipvault-daemon] Monitorando clipboard a cada {}ms (TTL de segredos: {}s).",
        watcher::POLL_INTERVAL.as_millis(),
        classifier::SECRET_TTL_SECONDS
    );

    let mut last_gc = Instant::now();

    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(entry) => {
                let preview = entry.single_line_summary(false, 60);
                println!(
                    "[clipvault-daemon] #{} {} (usos: {}): {}",
                    entry.id,
                    entry.kind.tag(),
                    entry.use_count,
                    preview
                );
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if last_gc.elapsed() >= GC_INTERVAL {
            let now_ts = Utc::now().timestamp();
            if let Ok(purged) = storage.purge_expired(now_ts) {
                if purged > 0 {
                    println!(
                        "[clipvault-daemon] GC removeu {purged} segredo(s) expirado(s)."
                    );
                }
            }
            last_gc = Instant::now();
        }
    }

    stop_flag.store(true, Ordering::Relaxed);
    match handle.join() {
        Ok(res) => res,
        Err(_) => anyhow::bail!("thread do watcher encerrou de forma inesperada"),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let db_path = cli.db.unwrap_or_else(Storage::default_db_path);
    let storage = Arc::new(Storage::open(&db_path)?);

    match cli.command.unwrap_or(Commands::Ui) {
        Commands::Daemon => run_daemon(storage),
        Commands::Ui => tui::run(storage),
    }
}
