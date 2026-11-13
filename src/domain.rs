use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Categoria semântica atribuída automaticamente a um item do clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentKind {
    PlainText,
    Url,
    Json,
    CodeSnippet,
    ShellCommand,
    SecretToken,
}

impl ContentKind {
    /// Retorna a tag textual exibida na interface de terminal.
    pub fn tag(&self) -> &'static str {
        match self {
            ContentKind::PlainText => "[TEXT]",
            ContentKind::Url => "[URL]",
            ContentKind::Json => "[JSON]",
            ContentKind::CodeSnippet => "[CODE]",
            ContentKind::ShellCommand => "[CMD]",
            ContentKind::SecretToken => "[SECRET]",
        }
    }

    /// Indica se o conteúdo é um segredo sensível que exige mascaramento e TTL.
    pub fn is_secret(&self) -> bool {
        matches!(self, ContentKind::SecretToken)
    }
}

/// Representa uma entrada armazenada no histórico do clipboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipEntry {
    pub id: u64,
    pub content: String,
    pub pretty_json: Option<String>,
    pub kind: ContentKind,
    pub created_at: i64,
    pub updated_at: i64,
    pub content_hash: String,
    pub use_count: u64,
    pub pinned: bool,
    pub expires_at: Option<i64>,
}

impl ClipEntry {
    /// Calcula o digest SHA-256 em hexadecimal para deduplicação de conteúdo.
    pub fn compute_hash(content: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        hex::encode(hasher.finalize())
    }

    /// Verifica se o item possui TTL configurado e já expirou no instante `now_ts`.
    pub fn is_expired(&self, now_ts: i64) -> bool {
        self.expires_at.is_some_and(|exp| now_ts >= exp)
    }

    /// Retorna o texto completo para o painel de preview.
    /// Segredos são mascarados enquanto `reveal_secret` for `false`,
    /// e JSON válido retorna sua versão formatada quando disponível.
    pub fn display_preview(&self, reveal_secret: bool) -> String {
        if self.kind.is_secret() && !reveal_secret {
            return "••••••••••••••••••••••••••••••••\n[SEGREDOS OCULTOS POR SEGURANÇA - Pressione 's' para revelar]"
                .to_string();
        }

        if self.kind == ContentKind::Json {
            return self
                .pretty_json
                .as_deref()
                .unwrap_or(&self.content)
                .to_string();
        }

        self.content.clone()
    }

    /// Retorna um resumo em linha única seguro para listagem na TUI.
    pub fn single_line_summary(&self, reveal_secret: bool, max_chars: usize) -> String {
        if self.kind.is_secret() && !reveal_secret {
            return "•••••••••••• [Token/Segredo Protegido]".to_string();
        }

        let normalized: String = self
            .content
            .chars()
            .map(|c| match c {
                '\n' | '\r' | '\t' => ' ',
                other => other,
            })
            .collect();
        let trimmed = normalized.split_whitespace().collect::<Vec<_>>().join(" ");

        let char_count = trimmed.chars().count();
        if char_count <= max_chars {
            return trimmed;
        }

        if max_chars <= 3 {
            return trimmed.chars().take(max_chars).collect();
        }

        let prefix: String = trimmed.chars().take(max_chars - 3).collect();
        format!("{prefix}...")
    }

    /// Retorna uma descrição amigável do tempo relativo desde `updated_at`.
    pub fn relative_time(&self, now_ts: i64) -> String {
        let elapsed = now_ts.saturating_sub(self.updated_at);
        if elapsed < 5 {
            "agora".to_string()
        } else if elapsed < 60 {
            format!("há {elapsed}s")
        } else if elapsed < 3600 {
            format!("há {}m", elapsed / 60)
        } else if elapsed < 86400 {
            format!("há {}h", elapsed / 3600)
        } else {
            format!("há {}d", elapsed / 86400)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(kind: ContentKind, content: &str) -> ClipEntry {
        ClipEntry {
            id: 1,
            content: content.to_string(),
            pretty_json: None,
            kind,
            created_at: 1_000,
            updated_at: 1_000,
            content_hash: ClipEntry::compute_hash(content),
            use_count: 1,
            pinned: false,
            expires_at: None,
        }
    }

    #[test]
    fn hash_is_deterministic_sha256() {
        let h1 = ClipEntry::compute_hash("hello clipvault");
        let h2 = ClipEntry::compute_hash("hello clipvault");
        let h3 = ClipEntry::compute_hash("hello clipvault!");
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn secret_preview_and_summary_are_masked_until_revealed() {
        let secret = sample_entry(ContentKind::SecretToken, "sk-1234567890abcdef123456");
        assert!(secret.display_preview(false).contains("OCULTOS"));
        assert!(!secret.display_preview(false).contains("sk-1234"));
        assert_eq!(
            secret.single_line_summary(false, 40),
            "•••••••••••• [Token/Segredo Protegido]"
        );
        assert_eq!(secret.display_preview(true), "sk-1234567890abcdef123456");
        assert_eq!(
            secret.single_line_summary(true, 40),
            "sk-1234567890abcdef123456"
        );
    }

    #[test]
    fn expiration_and_relative_time_work_as_expected() {
        let mut entry = sample_entry(ContentKind::SecretToken, "ghp_12345678901234567890");
        entry.expires_at = Some(1_300);

        assert!(!entry.is_expired(1_299));
        assert!(entry.is_expired(1_300));
        assert!(entry.is_expired(1_500));

        assert_eq!(entry.relative_time(1_002), "agora");
        assert_eq!(entry.relative_time(1_045), "há 45s");
        assert_eq!(entry.relative_time(1_180), "há 3m");
        assert_eq!(entry.relative_time(8_200), "há 2h");
        assert_eq!(entry.relative_time(173_800), "há 2d");
    }
}
