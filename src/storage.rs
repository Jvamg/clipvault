use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use redb::{Database, ReadableTable, TableDefinition};

use crate::classifier::{self, SECRET_TTL_SECONDS};
use crate::domain::ClipEntry;

const ENTRIES_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("entries");
const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const META_NEXT_ID_KEY: &str = "next_id";
const META_LAST_RESTORED_HASH_KEY: &str = "last_restored_hash";

/// Camada de persistência local embutida sobre `redb`.
pub struct Storage {
    db: Database,
}

impl Storage {
    /// Abre ou cria o banco `redb` no caminho especificado.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path_ref = path.as_ref();
        if let Some(parent) = path_ref.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("falha ao criar diretório do banco: {}", parent.display())
                })?;
            }
        }

        let db = Database::create(path_ref)
            .with_context(|| format!("falha ao abrir banco redb: {}", path_ref.display()))?;

        let write_txn = db.begin_write()?;
        {
            let _ = write_txn.open_table(ENTRIES_TABLE)?;
            let _ = write_txn.open_table(META_TABLE)?;
        }
        write_txn.commit()?;

        Ok(Self { db })
    }

    /// Caminho padrão do banco local (`~/.local/share/clipvault/clipvault.redb`).
    pub fn default_db_path() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("clipvault")
            .join("clipvault.redb")
    }

    /// Insere um novo item ou deduplica pelo SHA-256 se já existir.
    /// Também remove automaticamente itens expirados antes da gravação.
    pub fn insert_or_update(&self, content: &str, now_ts: i64) -> Result<Option<ClipEntry>> {
        if content.trim().is_empty() {
            return Ok(None);
        }

        let target_hash = ClipEntry::compute_hash(content);
        let write_txn = self.db.begin_write()?;

        let saved_entry = {
            let mut entries_table = write_txn.open_table(ENTRIES_TABLE)?;
            let mut meta_table = write_txn.open_table(META_TABLE)?;

            Self::purge_expired_in_table(&mut entries_table, now_ts)?;

            let mut existing_match: Option<ClipEntry> = None;
            {
                let iter = entries_table.iter()?;
                for item in iter {
                    let (_, raw_val) = item?;
                    let entry: ClipEntry = serde_json::from_slice(raw_val.value())?;
                    if entry.content_hash == target_hash {
                        existing_match = Some(entry);
                        break;
                    }
                }
            }

            let entry = if let Some(mut existing) = existing_match {
                existing.updated_at = now_ts;
                existing.use_count = existing.use_count.saturating_add(1);
                if existing.kind.is_secret() {
                    existing.expires_at = Some(now_ts + SECRET_TTL_SECONDS);
                }
                existing
            } else {
                let next_id = match meta_table.get(META_NEXT_ID_KEY)? {
                    Some(raw) => {
                        let bytes: [u8; 8] = raw
                            .value()
                            .try_into()
                            .context("next_id corrompido na tabela meta")?;
                        u64::from_le_bytes(bytes)
                    }
                    None => 1,
                };
                meta_table.insert(META_NEXT_ID_KEY, (next_id + 1).to_le_bytes().as_slice())?;
                classifier::build_entry(next_id, content, now_ts)
            };

            let encoded = serde_json::to_vec(&entry)?;
            entries_table.insert(entry.id, encoded.as_slice())?;
            entry
        };

        write_txn.commit()?;
        Ok(Some(saved_entry))
    }

    /// Busca uma entrada específica pelo ID.
    #[allow(dead_code)]
    pub fn get(&self, id: u64) -> Result<Option<ClipEntry>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ENTRIES_TABLE)?;
        match table.get(id)? {
            Some(raw) => Ok(Some(serde_json::from_slice(raw.value())?)),
            None => Ok(None),
        }
    }

    /// Lista todos os itens válidos ordenados por fixados no topo, depois mais recentes.
    pub fn list_all(&self, now_ts: i64) -> Result<Vec<ClipEntry>> {
        let entries = self.load_active_entries(now_ts)?;
        Ok(Self::sort_default(entries))
    }

    /// Alterna o estado `pinned` de um item existente.
    pub fn toggle_pin(&self, id: u64) -> Result<Option<ClipEntry>> {
        let write_txn = self.db.begin_write()?;
        let updated = {
            let mut table = write_txn.open_table(ENTRIES_TABLE)?;
            let current = match table.get(id)? {
                Some(raw) => Some(serde_json::from_slice::<ClipEntry>(raw.value())?),
                None => None,
            };

            if let Some(mut entry) = current {
                entry.pinned = !entry.pinned;
                let encoded = serde_json::to_vec(&entry)?;
                table.insert(id, encoded.as_slice())?;
                Some(entry)
            } else {
                None
            }
        };
        write_txn.commit()?;
        Ok(updated)
    }

    /// Remove um item pelo ID. Retorna `true` se o item existia.
    pub fn delete(&self, id: u64) -> Result<bool> {
        let write_txn = self.db.begin_write()?;
        let existed = {
            let mut table = write_txn.open_table(ENTRIES_TABLE)?;
            let removed = table.remove(id)?.is_some();
            removed
        };
        write_txn.commit()?;
        Ok(existed)
    }

    /// Registra que o usuário restaurou este item para o clipboard do sistema,
    /// salvando seu hash na tabela meta para o watcher ignorar o evento de eco.
    pub fn record_restore(&self, id: u64, now_ts: i64) -> Result<Option<ClipEntry>> {
        let write_txn = self.db.begin_write()?;
        let updated = {
            let mut entries_table = write_txn.open_table(ENTRIES_TABLE)?;
            let mut meta_table = write_txn.open_table(META_TABLE)?;

            let current = match entries_table.get(id)? {
                Some(raw) => Some(serde_json::from_slice::<ClipEntry>(raw.value())?),
                None => None,
            };

            match current {
                Some(mut entry) if !entry.is_expired(now_ts) => {
                    entry.use_count = entry.use_count.saturating_add(1);
                    entry.updated_at = now_ts;
                    let encoded = serde_json::to_vec(&entry)?;
                    entries_table.insert(id, encoded.as_slice())?;
                    meta_table.insert(META_LAST_RESTORED_HASH_KEY, entry.content_hash.as_bytes())?;
                    Some(entry)
                }
                Some(_) => {
                    entries_table.remove(id)?;
                    None
                }
                None => None,
            }
        };
        write_txn.commit()?;
        Ok(updated)
    }

    /// Lê e consome o último hash restaurado pela TUI para evitar loop no watcher.
    pub fn consume_last_restored_hash(&self) -> Result<Option<String>> {
        {
            let read_txn = self.db.begin_read()?;
            let meta_table = read_txn.open_table(META_TABLE)?;
            if meta_table.get(META_LAST_RESTORED_HASH_KEY)?.is_none() {
                return Ok(None);
            }
        }

        let write_txn = self.db.begin_write()?;
        let hash = {
            let mut meta_table = write_txn.open_table(META_TABLE)?;
            let found = match meta_table.remove(META_LAST_RESTORED_HASH_KEY)? {
                Some(raw) => Some(String::from_utf8(raw.value().to_vec())?),
                None => None,
            };
            found
        };
        write_txn.commit()?;
        Ok(hash)
    }

    /// Remove todas as entradas cujo TTL já expirou em `now_ts`.
    pub fn purge_expired(&self, now_ts: i64) -> Result<usize> {
        let write_txn = self.db.begin_write()?;
        let removed = {
            let mut table = write_txn.open_table(ENTRIES_TABLE)?;
            Self::purge_expired_in_table(&mut table, now_ts)?
        };
        write_txn.commit()?;
        Ok(removed)
    }

    /// Executa busca fuzzy nos itens ativos ordenando por maior pontuação de relevância.
    pub fn search(&self, query: &str, now_ts: i64) -> Result<Vec<ClipEntry>> {
        let trimmed = query.trim();
        let active = self.load_active_entries(now_ts)?;
        if trimmed.is_empty() {
            return Ok(Self::sort_default(active));
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
        let mut active = Vec::new();
        let mut has_expired = false;

        {
            let read_txn = self.db.begin_read()?;
            let table = read_txn.open_table(ENTRIES_TABLE)?;
            for item in table.iter()? {
                let (_, raw_val) = item?;
                let entry: ClipEntry = serde_json::from_slice(raw_val.value())?;
                if entry.is_expired(now_ts) {
                    has_expired = true;
                } else {
                    active.push(entry);
                }
            }
        }

        if has_expired {
            let _ = self.purge_expired(now_ts)?;
        }

        Ok(active)
    }

    fn purge_expired_in_table(
        table: &mut redb::Table<u64, &[u8]>,
        now_ts: i64,
    ) -> Result<usize> {
        let mut expired_ids = Vec::new();
        {
            let iter = table.iter()?;
            for item in iter {
                let (key, raw_val) = item?;
                let entry: ClipEntry = serde_json::from_slice(raw_val.value())?;
                if entry.is_expired(now_ts) {
                    expired_ids.push(key.value());
                }
            }
        }

        let count = expired_ids.len();
        for id in expired_ids {
            table.remove(id)?;
        }
        Ok(count)
    }

    fn sort_default(mut entries: Vec<ClipEntry>) -> Vec<ClipEntry> {
        entries.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then_with(|| b.updated_at.cmp(&a.updated_at))
                .then_with(|| b.id.cmp(&a.id))
        });
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ContentKind;
    use tempfile::tempdir;

    fn test_storage() -> (tempfile::TempDir, Storage) {
        let dir = tempdir().expect("temp dir");
        let db_path = dir.path().join("test_clipvault.redb");
        let storage = Storage::open(&db_path).expect("open storage");
        (dir, storage)
    }

    #[test]
    fn crud_and_persistence_work() {
        let dir = tempdir().expect("temp dir");
        let db_path = dir.path().join("persist.redb");

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
            .insert_or_update("clipvault daemon --db local.redb", 150)
            .expect("insert")
            .expect("entry");

        let results = storage.search("clipvault", 300).expect("search");
        assert!(!results.is_empty());
        assert_eq!(results[0].id, exact.id);
    }
}
