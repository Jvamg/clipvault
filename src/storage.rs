use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use rusqlite::types::Type;
use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};

use crate::classifier::{self, SECRET_TTL_SECONDS};
use crate::domain::{ClipEntry, ContentKind};

const META_LAST_RESTORED_HASH_KEY: &str = "last_restored_hash";
const ENTRY_SELECT_COLS: &str = "id, content, pretty_json, kind, created_at, updated_at, content_hash, use_count, pinned, expires_at, image_blob, image_width, image_height";

/// Camada de persistência local embutida sobre SQLite (`rusqlite`) em modo WAL.
pub struct Storage {
    conn: Mutex<Connection>,
}

impl Storage {
    /// Abre ou cria o banco SQLite no caminho especificado configurando WAL,
    /// sincronização e timeout para acesso concorrente entre `daemon` e `ui`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path_ref = path.as_ref();
        if let Some(parent) = path_ref.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("falha ao criar diretório do banco: {}", parent.display())
                })?;
            }
        }

        let conn = Connection::open(path_ref)
            .with_context(|| format!("falha ao abrir banco sqlite: {}", path_ref.display()))?;

        // Garante timeout antes de executar PRAGMAs e criação de schema sob concorrência.
        conn.busy_timeout(Duration::from_millis(5000))
            .context("falha ao configurar busy_timeout no SQLite")?;

        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA busy_timeout = 5000;

            CREATE TABLE IF NOT EXISTS entries (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                content TEXT NOT NULL,
                pretty_json TEXT,
                kind TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                content_hash TEXT NOT NULL UNIQUE,
                use_count INTEGER NOT NULL DEFAULT 1,
                pinned INTEGER NOT NULL DEFAULT 0,
                expires_at INTEGER,
                image_blob BLOB,
                image_width INTEGER,
                image_height INTEGER
            );

            CREATE UNIQUE INDEX IF NOT EXISTS idx_entries_content_hash
                ON entries(content_hash);

            CREATE INDEX IF NOT EXISTS idx_entries_sort
                ON entries(pinned DESC, updated_at DESC, id DESC);

            CREATE INDEX IF NOT EXISTS idx_entries_expires_at
                ON entries(expires_at)
                WHERE expires_at IS NOT NULL;

            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            ",
        )
        .context("falha ao inicializar PRAGMAs e schema SQLite")?;

        ensure_image_columns(&conn)?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Caminho padrão do banco local (`~/.local/share/clipvault/clipvault.db`).
    pub fn default_db_path() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("clipvault")
            .join("clipvault.db")
    }

    /// Insere um novo item ou deduplica pelo SHA-256 se já existir.
    /// Também remove automaticamente itens expirados antes da gravação.
    pub fn insert_or_update(&self, content: &str, now_ts: i64) -> Result<Option<ClipEntry>> {
        if content.trim().is_empty() {
            return Ok(None);
        }

        let target_hash = ClipEntry::compute_hash(content);
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        tx.execute(
            "DELETE FROM entries WHERE expires_at IS NOT NULL AND expires_at <= ?1",
            params![now_ts],
        )?;

        let existing = tx
            .query_row(
                &format!("SELECT {ENTRY_SELECT_COLS} FROM entries WHERE content_hash = ?1"),
                params![target_hash],
                row_to_entry,
            )
            .optional()?;

        let saved_entry = if let Some(mut entry) = existing {
            entry.updated_at = now_ts;
            entry.use_count = entry.use_count.saturating_add(1);
            if entry.kind.is_secret() {
                entry.expires_at = Some(now_ts + SECRET_TTL_SECONDS);
            }

            tx.execute(
                "UPDATE entries
                 SET updated_at = ?1, use_count = ?2, expires_at = ?3
                 WHERE id = ?4",
                params![
                    entry.updated_at,
                    entry.use_count as i64,
                    entry.expires_at,
                    entry.id as i64
                ],
            )?;
            entry
        } else {
            let mut entry = classifier::build_entry(0, content, now_ts);
            tx.execute(
                "INSERT INTO entries (
                    content, pretty_json, kind, created_at, updated_at,
                    content_hash, use_count, pinned, expires_at,
                    image_blob, image_width, image_height
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, NULL, NULL)",
                params![
                    entry.content,
                    entry.pretty_json,
                    kind_to_str(entry.kind),
                    entry.created_at,
                    entry.updated_at,
                    entry.content_hash,
                    entry.use_count as i64,
                    i64::from(entry.pinned),
                    entry.expires_at,
                ],
            )?;
            entry.id = tx.last_insert_rowid() as u64;
            entry
        };

        tx.commit()?;
        Ok(Some(saved_entry))
    }

    /// Insere uma nova imagem PNG ou deduplica pelo hash SHA-256 dos bytes da imagem.
    pub fn insert_or_update_image(
        &self,
        png_bytes: Vec<u8>,
        width: u32,
        height: u32,
        now_ts: i64,
    ) -> Result<Option<ClipEntry>> {
        if png_bytes.is_empty() || width == 0 || height == 0 {
            return Ok(None);
        }

        let target_hash = ClipEntry::compute_bytes_hash(&png_bytes);
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        tx.execute(
            "DELETE FROM entries WHERE expires_at IS NOT NULL AND expires_at <= ?1",
            params![now_ts],
        )?;

        let existing = tx
            .query_row(
                &format!("SELECT {ENTRY_SELECT_COLS} FROM entries WHERE content_hash = ?1"),
                params![target_hash],
                row_to_entry,
            )
            .optional()?;

        let saved_entry = if let Some(mut entry) = existing {
            entry.updated_at = now_ts;
            entry.use_count = entry.use_count.saturating_add(1);
            tx.execute(
                "UPDATE entries SET updated_at = ?1, use_count = ?2 WHERE id = ?3",
                params![entry.updated_at, entry.use_count as i64, entry.id as i64],
            )?;
            entry
        } else {
            let mut entry = ClipEntry::new_image(0, png_bytes, width, height, now_ts);
            tx.execute(
                "INSERT INTO entries (
                    content, pretty_json, kind, created_at, updated_at,
                    content_hash, use_count, pinned, expires_at,
                    image_blob, image_width, image_height
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    entry.content,
                    entry.pretty_json,
                    kind_to_str(entry.kind),
                    entry.created_at,
                    entry.updated_at,
                    entry.content_hash,
                    entry.use_count as i64,
                    i64::from(entry.pinned),
                    entry.expires_at,
                    entry.image_blob,
                    i64::from(width),
                    i64::from(height),
                ],
            )?;
            entry.id = tx.last_insert_rowid() as u64;
            entry
        };

        tx.commit()?;
        Ok(Some(saved_entry))
    }

    /// Busca uma entrada específica pelo ID.
    #[allow(dead_code)]
    pub fn get(&self, id: u64) -> Result<Option<ClipEntry>> {
        let conn = self.lock_conn()?;
        let entry = conn
            .query_row(
                &format!("SELECT {ENTRY_SELECT_COLS} FROM entries WHERE id = ?1"),
                params![id as i64],
                row_to_entry,
            )
            .optional()?;
        Ok(entry)
    }

    /// Lista todos os itens válidos ordenados por fixados no topo, depois mais recentes.
    pub fn list_all(&self, now_ts: i64) -> Result<Vec<ClipEntry>> {
        self.load_active_entries(now_ts)
    }

    /// Alterna o estado `pinned` de um item existente.
    pub fn toggle_pin(&self, id: u64) -> Result<Option<ClipEntry>> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let current = tx
            .query_row(
                &format!("SELECT {ENTRY_SELECT_COLS} FROM entries WHERE id = ?1"),
                params![id as i64],
                row_to_entry,
            )
            .optional()?;

        let updated = if let Some(mut entry) = current {
            entry.pinned = !entry.pinned;
            tx.execute(
                "UPDATE entries SET pinned = ?1 WHERE id = ?2",
                params![i64::from(entry.pinned), id as i64],
            )?;
            Some(entry)
        } else {
            None
        };

        tx.commit()?;
        Ok(updated)
    }

    /// Remove um item pelo ID. Retorna `true` se o item existia.
    pub fn delete(&self, id: u64) -> Result<bool> {
        let conn = self.lock_conn()?;
        let removed = conn.execute("DELETE FROM entries WHERE id = ?1", params![id as i64])?;
        Ok(removed > 0)
    }

    /// Registra que o usuário restaurou este item para o clipboard do sistema,
    /// salvando seu hash na tabela meta para o watcher ignorar o evento de eco.
    pub fn record_restore(&self, id: u64, now_ts: i64) -> Result<Option<ClipEntry>> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let current = tx
            .query_row(
                &format!("SELECT {ENTRY_SELECT_COLS} FROM entries WHERE id = ?1"),
                params![id as i64],
                row_to_entry,
            )
            .optional()?;

        let updated = match current {
            Some(mut entry) if !entry.is_expired(now_ts) => {
                entry.use_count = entry.use_count.saturating_add(1);
                entry.updated_at = now_ts;
                tx.execute(
                    "UPDATE entries SET use_count = ?1, updated_at = ?2 WHERE id = ?3",
                    params![entry.use_count as i64, entry.updated_at, id as i64],
                )?;
                tx.execute(
                    "INSERT INTO meta (key, value) VALUES (?1, ?2)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![META_LAST_RESTORED_HASH_KEY, entry.content_hash],
                )?;
                Some(entry)
            }
            Some(_) => {
                tx.execute("DELETE FROM entries WHERE id = ?1", params![id as i64])?;
                None
            }
            None => None,
        };

        tx.commit()?;
        Ok(updated)
    }

    /// Lê e consome o último hash restaurado pela TUI para evitar loop no watcher.
    pub fn consume_last_restored_hash(&self) -> Result<Option<String>> {
        let mut conn = self.lock_conn()?;

        let exists: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![META_LAST_RESTORED_HASH_KEY],
                |row| row.get(0),
            )
            .optional()?;

        if exists.is_none() {
            return Ok(None);
        }

        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value: Option<String> = tx
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![META_LAST_RESTORED_HASH_KEY],
                |row| row.get(0),
            )
            .optional()?;

        if value.is_some() {
            tx.execute(
                "DELETE FROM meta WHERE key = ?1",
                params![META_LAST_RESTORED_HASH_KEY],
            )?;
        }
        tx.commit()?;
        Ok(value)
    }

    /// Remove todas as entradas cujo TTL já expirou em `now_ts`.
    pub fn purge_expired(&self, now_ts: i64) -> Result<usize> {
        let conn = self.lock_conn()?;
        let removed = conn.execute(
            "DELETE FROM entries WHERE expires_at IS NOT NULL AND expires_at <= ?1",
            params![now_ts],
        )?;
        Ok(removed)
    }

    /// Executa busca fuzzy nos itens ativos ordenando por maior pontuação de relevância.
    pub fn search(&self, query: &str, now_ts: i64) -> Result<Vec<ClipEntry>> {
        let trimmed = query.trim();
        let active = self.load_active_entries(now_ts)?;
        if trimmed.is_empty() {
            return Ok(active);
        }

        let matcher = SkimMatcherV2::default();
        let mut scored: Vec<(i64, ClipEntry)> = active
            .into_iter()
            .filter_map(|entry| {
                let haystack = format!("{} {}", entry.kind.tag(), entry.content);
                matcher
                    .fuzzy_match(&haystack, trimmed)
                    .map(|score| (score, entry))
            })
            .collect();

        scored.sort_by(|(score_a, entry_a), (score_b, entry_b)| {
            score_b
                .cmp(score_a)
                .then_with(|| entry_b.pinned.cmp(&entry_a.pinned))
                .then_with(|| entry_b.updated_at.cmp(&entry_a.updated_at))
                .then_with(|| entry_b.id.cmp(&entry_a.id))
        });

        Ok(scored.into_iter().map(|(_, entry)| entry).collect())
    }

    fn load_active_entries(&self, now_ts: i64) -> Result<Vec<ClipEntry>> {
        let conn = self.lock_conn()?;

        let has_expired: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM entries WHERE expires_at IS NOT NULL AND expires_at <= ?1)",
            params![now_ts],
            |row| row.get::<_, i64>(0).map(|v| v != 0),
        )?;

        if has_expired {
            conn.execute(
                "DELETE FROM entries WHERE expires_at IS NOT NULL AND expires_at <= ?1",
                params![now_ts],
            )?;
        }

        let mut stmt = conn.prepare(&format!(
            "SELECT {ENTRY_SELECT_COLS}
             FROM entries
             WHERE expires_at IS NULL OR expires_at > ?1
             ORDER BY pinned DESC, updated_at DESC, id DESC"
        ))?;

        let rows = stmt.query_map(params![now_ts], row_to_entry)?;
        let mut active = Vec::new();
        for row in rows {
            active.push(row?);
        }
        Ok(active)
    }

    fn lock_conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| anyhow::anyhow!("mutex da conexão SQLite envenenado"))
    }
}

fn ensure_image_columns(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(entries);")?;
    let existing_cols: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    if !existing_cols.iter().any(|c| c == "image_blob") {
        conn.execute("ALTER TABLE entries ADD COLUMN image_blob BLOB;", [])?;
    }
    if !existing_cols.iter().any(|c| c == "image_width") {
        conn.execute("ALTER TABLE entries ADD COLUMN image_width INTEGER;", [])?;
    }
    if !existing_cols.iter().any(|c| c == "image_height") {
        conn.execute("ALTER TABLE entries ADD COLUMN image_height INTEGER;", [])?;
    }

    Ok(())
}

fn kind_to_str(kind: ContentKind) -> &'static str {
    match kind {
        ContentKind::PlainText => "PlainText",
        ContentKind::Url => "Url",
        ContentKind::Json => "Json",
        ContentKind::CodeSnippet => "CodeSnippet",
        ContentKind::ShellCommand => "ShellCommand",
        ContentKind::SecretToken => "SecretToken",
        ContentKind::Image => "Image",
    }
}

fn kind_from_str(raw: &str) -> std::result::Result<ContentKind, rusqlite::Error> {
    match raw {
        "PlainText" => Ok(ContentKind::PlainText),
        "Url" => Ok(ContentKind::Url),
        "Json" => Ok(ContentKind::Json),
        "CodeSnippet" => Ok(ContentKind::CodeSnippet),
        "ShellCommand" => Ok(ContentKind::ShellCommand),
        "SecretToken" => Ok(ContentKind::SecretToken),
        "Image" => Ok(ContentKind::Image),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            3,
            Type::Text,
            format!("ContentKind inválido no banco: {other}").into(),
        )),
    }
}

fn row_to_entry(row: &Row<'_>) -> rusqlite::Result<ClipEntry> {
    let kind_raw: String = row.get(3)?;
    let kind = kind_from_str(&kind_raw)?;
    let image_blob: Option<Vec<u8>> = row.get(10)?;
    let image_width: Option<i64> = row.get(11)?;
    let image_height: Option<i64> = row.get(12)?;
    let image_dimensions = match (image_width, image_height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Some((w as u32, h as u32)),
        _ => None,
    };

    Ok(ClipEntry {
        id: row.get::<_, i64>(0)? as u64,
        content: row.get(1)?,
        pretty_json: row.get(2)?,
        kind,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
        content_hash: row.get(6)?,
        use_count: row.get::<_, i64>(7)? as u64,
        pinned: row.get::<_, i64>(8)? != 0,
        expires_at: row.get(9)?,
        image_blob,
        image_dimensions,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;
    use crate::domain::ContentKind;
    use tempfile::tempdir;

    fn test_storage() -> (tempfile::TempDir, Storage) {
        let dir = tempdir().expect("temp dir");
        let db_path = dir.path().join("test_clipvault.db");
        let storage = Storage::open(&db_path).expect("open storage");
        (dir, storage)
    }

    #[test]
    fn crud_and_persistence_work() {
        let dir = tempdir().expect("temp dir");
        let db_path = dir.path().join("persist.db");

        let id = {
            let storage = Storage::open(&db_path).expect("open");
            assert!(storage
                .insert_or_update("   \n\t ", 1_000)
                .expect("blank ignored")
                .is_none());

            let created = storage
                .insert_or_update("https://rust-lang.org", 1_000)
                .expect("insert")
                .expect("entry");
            assert_eq!(created.kind, ContentKind::Url);
            assert_eq!(created.use_count, 1);
            created.id
        };

        let reopened = Storage::open(&db_path).expect("reopen");
        let fetched = reopened.get(id).expect("get").expect("exists");
        assert_eq!(fetched.content, "https://rust-lang.org");

        assert!(reopened.delete(id).expect("delete"));
        assert!(!reopened.delete(id).expect("delete missing"));
        assert!(reopened.get(id).expect("get after delete").is_none());
    }

    #[test]
    fn deduplicates_by_sha256_and_updates_timestamp_and_use_count() {
        let (_dir, storage) = test_storage();

        let first = storage
            .insert_or_update("cargo build --release", 100)
            .expect("first insert")
            .expect("entry");
        let other = storage
            .insert_or_update("nota qualquer", 110)
            .expect("other insert")
            .expect("entry");
        let dup = storage
            .insert_or_update("cargo build --release", 200)
            .expect("dup insert")
            .expect("entry");

        assert_eq!(first.id, dup.id);
        assert_eq!(dup.created_at, 100);
        assert_eq!(dup.updated_at, 200);
        assert_eq!(dup.use_count, 2);

        let listed = storage.list_all(200).expect("list");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, first.id);
        assert_eq!(listed[1].id, other.id);
    }

    #[test]
    fn secret_tokens_expire_after_five_minutes() {
        let (_dir, storage) = test_storage();
        let now = 10_000;

        let secret = storage
            .insert_or_update("sk-secretkey1234567890abcdef", now)
            .expect("insert secret")
            .expect("entry");
        let normal = storage
            .insert_or_update("texto comum seguro", now)
            .expect("insert text")
            .expect("entry");

        assert_eq!(secret.kind, ContentKind::SecretToken);
        assert_eq!(secret.expires_at, Some(now + 300));

        // Ainda válido aos 299s
        let active_before = storage.list_all(now + 299).expect("list before expiry");
        assert_eq!(active_before.len(), 2);

        // Expirado aos 300s -> removido automaticamente
        let active_after = storage.list_all(now + 300).expect("list after expiry");
        assert_eq!(active_after.len(), 1);
        assert_eq!(active_after[0].id, normal.id);
        assert!(storage.get(secret.id).expect("get purged").is_none());
    }

    #[test]
    fn toggle_pin_keeps_item_at_top_and_restore_records_meta_hash() {
        let (_dir, storage) = test_storage();

        let older = storage
            .insert_or_update("item antigo fixado", 100)
            .expect("insert")
            .expect("entry");
        let newer = storage
            .insert_or_update("item recente normal", 200)
            .expect("insert")
            .expect("entry");

        let pinned = storage
            .toggle_pin(older.id)
            .expect("toggle")
            .expect("exists");
        assert!(pinned.pinned);

        let list = storage.list_all(250).expect("list");
        assert_eq!(list[0].id, older.id);
        assert_eq!(list[1].id, newer.id);

        let restored = storage
            .record_restore(older.id, 300)
            .expect("restore")
            .expect("entry");
        assert_eq!(restored.use_count, 2);
        assert_eq!(
            storage
                .consume_last_restored_hash()
                .expect("consume")
                .as_deref(),
            Some(older.content_hash.as_str())
        );
        assert!(storage
            .consume_last_restored_hash()
            .expect("consume empty")
            .is_none());
    }

    #[test]
    fn fuzzy_search_orders_by_relevance_score() {
        let (_dir, storage) = test_storage();

        storage
            .insert_or_update("configuracao geral de rede", 100)
            .expect("insert");
        storage
            .insert_or_update("c l i p _ v a u l t _ outro", 200)
            .expect("insert");
        let exact = storage
            .insert_or_update("clipvault daemon --db local.db", 150)
            .expect("insert")
            .expect("entry");

        let results = storage.search("clipvault", 300).expect("search");
        assert!(!results.is_empty());
        assert_eq!(results[0].id, exact.id);
    }

    #[test]
    fn concurrent_access_between_two_storage_instances_on_same_db() {
        let dir = tempdir().expect("temp dir");
        let db_path = dir.path().join("concurrent.db");

        let storage_daemon = Arc::new(Storage::open(&db_path).expect("open daemon storage"));
        let storage_ui = Arc::new(Storage::open(&db_path).expect("open ui storage"));

        {
            let conn = storage_daemon.lock_conn().expect("lock");
            let mode: String = conn
                .query_row("PRAGMA journal_mode;", [], |row| row.get(0))
                .expect("read journal_mode");
            assert_eq!(mode.to_lowercase(), "wal");
        }

        let seed = storage_daemon
            .insert_or_update("seed clipvault concurrent entry", 1_000)
            .expect("seed insert")
            .expect("seed entry");

        let daemon_writer = Arc::clone(&storage_daemon);
        let daemon_handle = thread::spawn(move || {
            for idx in 0..30 {
                let text = format!("daemon clip item #{idx}");
                daemon_writer
                    .insert_or_update(&text, 1_001 + idx as i64)
                    .expect("concurrent daemon insert");
                let _ = daemon_writer
                    .consume_last_restored_hash()
                    .expect("concurrent consume hash");
                let _ = daemon_writer
                    .purge_expired(1_001 + idx as i64)
                    .expect("concurrent purge");
            }
        });

        let ui_worker = Arc::clone(&storage_ui);
        let ui_handle = thread::spawn(move || {
            for idx in 0..30 {
                let entries = ui_worker.list_all(2_000).expect("concurrent ui list");
                assert!(!entries.is_empty());
                let matches = ui_worker
                    .search("clip", 2_000)
                    .expect("concurrent ui search");
                assert!(!matches.is_empty());

                if idx == 10 {
                    ui_worker
                        .toggle_pin(seed.id)
                        .expect("concurrent ui pin");
                }
                if idx == 20 {
                    ui_worker
                        .record_restore(seed.id, 1_500)
                        .expect("concurrent ui restore");
                }
            }
        });

        daemon_handle.join().expect("daemon thread should succeed");
        ui_handle.join().expect("ui thread should succeed");

        let final_entries = storage_ui.list_all(2_000).expect("final list");
        assert_eq!(final_entries.len(), 31);
        assert_eq!(final_entries[0].id, seed.id);
        assert!(final_entries[0].pinned);
        assert_eq!(final_entries[0].use_count, 2);
    }

    #[test]
    fn image_persistence_deduplication_and_restore_work() {
        let (_dir, storage) = test_storage();
        let png_bytes = vec![0x89, b'P', b'N', b'G', 1, 2, 3, 4, 5, 6];
        let expected_hash = ClipEntry::compute_bytes_hash(&png_bytes);

        let first = storage
            .insert_or_update_image(png_bytes.clone(), 1920, 1080, 1_000)
            .expect("insert image")
            .expect("entry");

        assert_eq!(first.kind, ContentKind::Image);
        assert_eq!(first.kind.tag(), "[IMG]");
        assert_eq!(first.image_dimensions, Some((1920, 1080)));
        assert_eq!(first.image_blob.as_deref(), Some(png_bytes.as_slice()));
        assert_eq!(first.content_hash, expected_hash);
        assert_eq!(first.content, "Imagem PNG (1920x1080, 1 KB)");
        assert_eq!(first.use_count, 1);

        // Mesmos bytes devem deduplicar sem criar segundo registro
        let dup = storage
            .insert_or_update_image(png_bytes.clone(), 1920, 1080, 1_200)
            .expect("dup image")
            .expect("entry");
        assert_eq!(dup.id, first.id);
        assert_eq!(dup.use_count, 2);
        assert_eq!(dup.updated_at, 1_200);

        let listed = storage.list_all(1_300).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].image_blob.as_deref(), Some(png_bytes.as_slice()));

        let restored = storage
            .record_restore(first.id, 1_400)
            .expect("restore image")
            .expect("entry");
        assert_eq!(restored.use_count, 3);
        assert_eq!(
            storage.consume_last_restored_hash().expect("consume"),
            Some(expected_hash)
        );
    }

    #[test]
    fn legacy_sqlite_schema_migrates_without_losing_data() {
        let dir = tempdir().expect("temp dir");
        let db_path = dir.path().join("legacy.db");

        {
            let conn = Connection::open(&db_path).expect("open legacy db");
            conn.execute_batch(
                "
                CREATE TABLE entries (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    content TEXT NOT NULL,
                    pretty_json TEXT,
                    kind TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    content_hash TEXT NOT NULL UNIQUE,
                    use_count INTEGER NOT NULL DEFAULT 1,
                    pinned INTEGER NOT NULL DEFAULT 0,
                    expires_at INTEGER
                );
                INSERT INTO entries (
                    content, pretty_json, kind, created_at, updated_at,
                    content_hash, use_count, pinned, expires_at
                ) VALUES (
                    'historico antigo preservado', NULL, 'PlainText',
                    100, 100, 'legacyhash123', 1, 0, NULL
                );
                ",
            )
            .expect("seed legacy table");
        }

        let storage = Storage::open(&db_path).expect("migrate legacy db");
        let existing = storage.list_all(200).expect("list migrated");
        assert_eq!(existing.len(), 1);
        assert_eq!(existing[0].content, "historico antigo preservado");
        assert!(existing[0].image_blob.is_none());
        assert!(existing[0].image_dimensions.is_none());

        let img = storage
            .insert_or_update_image(vec![1, 2, 3, 4], 64, 64, 250)
            .expect("insert image after migration")
            .expect("img entry");
        assert_eq!(img.kind, ContentKind::Image);
        assert_eq!(storage.list_all(260).expect("list all").len(), 2);
    }
}
