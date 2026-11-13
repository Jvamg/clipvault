use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Semantic category automatically assigned to a clipboard entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentKind {
    PlainText,
    Url,
    Json,
    CodeSnippet,
    ShellCommand,
    SecretToken,
    Image,
}

impl ContentKind {
    /// Returns the text tag displayed in the terminal UI.
    pub fn tag(&self) -> &'static str {
        match self {
            ContentKind::PlainText => "[TEXT]",
            ContentKind::Url => "[URL]",
            ContentKind::Json => "[JSON]",
            ContentKind::CodeSnippet => "[CODE]",
            ContentKind::ShellCommand => "[CMD]",
            ContentKind::SecretToken => "[SECRET]",
            ContentKind::Image => "[IMG]",
        }
    }

    /// Returns true if the content is a sensitive secret requiring masking and a TTL.
    pub fn is_secret(&self) -> bool {
        matches!(self, ContentKind::SecretToken)
    }
}

/// Represents a single entry stored in clipboard history.
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
    pub image_blob: Option<Vec<u8>>,
    pub image_dimensions: Option<(u32, u32)>,
}

impl ClipEntry {
    /// Computes the hexadecimal SHA-256 digest of an arbitrary byte slice.
    pub fn compute_bytes_hash(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex::encode(hasher.finalize())
    }

    /// Computes the hexadecimal SHA-256 digest for text content deduplication.
    pub fn compute_hash(content: &str) -> String {
        Self::compute_bytes_hash(content.as_bytes())
    }

    /// Formats a short human-readable description for a captured PNG image.
    pub fn format_image_description(width: u32, height: u32, byte_len: usize) -> String {
        let kb = byte_len.div_ceil(1024).max(1);
        format!("PNG Image ({width}x{height}, {kb} KB)")
    }

    /// Builds a `ClipEntry` for an encoded PNG image.
    pub fn new_image(
        id: u64,
        png_bytes: Vec<u8>,
        width: u32,
        height: u32,
        now_ts: i64,
    ) -> Self {
        let content = Self::format_image_description(width, height, png_bytes.len());
        let content_hash = Self::compute_bytes_hash(&png_bytes);
        Self {
            id,
            content,
            pretty_json: None,
            kind: ContentKind::Image,
            created_at: now_ts,
            updated_at: now_ts,
            content_hash,
            use_count: 1,
            pinned: false,
            expires_at: None,
            image_blob: Some(png_bytes),
            image_dimensions: Some((width, height)),
        }
    }

    /// Returns true if the entry has a TTL and has already expired at `now_ts`.
    pub fn is_expired(&self, now_ts: i64) -> bool {
        self.expires_at.is_some_and(|exp| now_ts >= exp)
    }

    /// Returns the full body text for the preview pane.
    /// Secrets remain masked while `reveal_secret` is `false`,
    /// and valid JSON returns its pretty-printed representation when available.
    pub fn display_preview(&self, reveal_secret: bool) -> String {
        if self.kind.is_secret() && !reveal_secret {
            return "••••••••••••••••••••••••••••••••\n[SECRET HIDDEN FOR SECURITY - Press 's' to reveal]"
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

    /// Returns a safe single-line summary for listing entries in the TUI.
    pub fn single_line_summary(&self, reveal_secret: bool, max_chars: usize) -> String {
        if self.kind.is_secret() && !reveal_secret {
            return "•••••••••••• [Protected Token/Secret]".to_string();
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

    /// Returns a friendly relative time description since `updated_at`.
    pub fn relative_time(&self, now_ts: i64) -> String {
        let elapsed = now_ts.saturating_sub(self.updated_at);
        if elapsed < 5 {
            "just now".to_string()
        } else if elapsed < 60 {
            format!("{elapsed}s ago")
        } else if elapsed < 3600 {
            format!("{}m ago", elapsed / 60)
        } else if elapsed < 86400 {
            format!("{}h ago", elapsed / 3600)
        } else {
            format!("{}d ago", elapsed / 86400)
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
            image_blob: None,
            image_dimensions: None,
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
        assert!(secret.display_preview(false).contains("HIDDEN"));
        assert!(!secret.display_preview(false).contains("sk-1234"));
        assert_eq!(
            secret.single_line_summary(false, 40),
            "•••••••••••• [Protected Token/Secret]"
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

        assert_eq!(entry.relative_time(1_002), "just now");
        assert_eq!(entry.relative_time(1_045), "45s ago");
        assert_eq!(entry.relative_time(1_180), "3m ago");
        assert_eq!(entry.relative_time(8_200), "2h ago");
        assert_eq!(entry.relative_time(173_800), "2d ago");
    }
}
