use std::sync::OnceLock;

use regex::Regex;

use crate::domain::{ClipEntry, ContentKind};

/// Tempo de vida automático (5 minutos) para tokens e segredos sensíveis.
pub const SECRET_TTL_SECONDS: i64 = 300;

/// Resultado da classificação heurística de um conteúdo copiado.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub kind: ContentKind,
    pub pretty_json: Option<String>,
    pub ttl_seconds: Option<i64>,
}

struct ClassifierPatterns {
    secret_patterns: Vec<Regex>,
    url_regex: Regex,
    code_patterns: Vec<Regex>,
    shell_shebang_or_prompt: Regex,
    shell_pipe_or_chain: Regex,
    shell_command_prefix: Regex,
}

fn patterns() -> &'static ClassifierPatterns {
    static PATTERNS: OnceLock<ClassifierPatterns> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let secret_patterns = vec![
            // Chaves OpenAI / Stripe / Anthropic (sk-...)
            Regex::new(r"\bsk-[A-Za-z0-9_\-]{16,}\b").expect("valid sk regex"),
            // Tokens GitHub clássicos e fine-grained
            Regex::new(r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}\b")
                .expect("valid github token regex"),
            Regex::new(r"\bgithub_pat_[A-Za-z0-9_]{20,}\b").expect("valid github pat regex"),
            // AWS Access Key ID
            Regex::new(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b").expect("valid aws regex"),
            // Slack tokens
            Regex::new(r"\bxox[baprs]-[0-9A-Za-z\-]{10,}\b").expect("valid slack regex"),
            // Google API Key
            Regex::new(r"\bAIza[0-9A-Za-z\-_]{35}\b").expect("valid google api key regex"),
            // Bearer tokens
            Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9\-._~+/]{16,}=*")
                .expect("valid bearer regex"),
            // JSON Web Tokens (JWT)
            Regex::new(r"\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b")
                .expect("valid jwt regex"),
            // Chaves privadas PEM / OpenSSH
            Regex::new(r"-----BEGIN (?:RSA |OPENSSH |EC |DSA |PGP )?PRIVATE KEY-----")
                .expect("valid private key regex"),
            // Senhas ou chaves explícitas em atribuições (env, config, JSON)
            Regex::new(
                r#"(?i)(?:^|[\s"'_,.;({])(?:password|passwd|pwd|secret|api[_-]?key|access[_-]?token|auth[_-]?token)['"]?\s*[:=]\s*['"]?[^\s'",}]{6,}['"]?"#,
            )
            .expect("valid password assignment regex"),
        ];

        let url_regex =
            Regex::new(r"^(?:https?|ftp|ws|wss|ssh)://[^\s/$.?#].[^\s]*$").expect("valid url regex");

        let code_patterns = vec![
            Regex::new(r"\b(?:pub\s+)?fn\s+[A-Za-z_]\w*\s*\(").expect("valid rust fn regex"),
            Regex::new(r"\blet\s+(?:mut\s+)?[A-Za-z_]\w*\s*(?::[^=]+)?=").expect("valid let regex"),
            Regex::new(r"\b(?:const|var)\s+[A-Za-z_]\w*\s*=").expect("valid js const/var regex"),
            Regex::new(r"\b(?:def|class|struct|impl|interface|enum|package)\s+[A-Za-z_]\w*")
                .expect("valid decl regex"),
            Regex::new(r"^\s*import\s+(?:\{|[\w.*]+)").expect("valid import regex"),
            Regex::new(r#"#include\s*[<"][^>"]+[>"]"#).expect("valid c include regex"),
            Regex::new(r"\bfunction\s+[A-Za-z_]\w*\s*\(").expect("valid js function regex"),
            Regex::new(r"=>").expect("valid arrow regex"),
            Regex::new(r"\b(?:console\.log|println!|eprintln!|fmt\.Println)\s*\(")
                .expect("valid print regex"),
        ];

        let shell_shebang_or_prompt =
            Regex::new(r"^(?:#!\s*(?:/\S*/)?(?:env\s+)?(?:ba|z)?sh\b|\$\s+\S+)")
                .expect("valid shebang/prompt regex");
        let shell_pipe_or_chain =
            Regex::new(r"(?:\s\|\s|\s&&\s|2>&1)").expect("valid pipe/chain regex");
        let shell_command_prefix = Regex::new(
            r"^(?:sudo|cargo|git|docker|kubectl|apt|apt-get|systemctl|journalctl|curl|wget|ssh|scp|rsync|tar|grep|rg|find|chmod|chown|export|npm|pnpm|yarn|pip|pip3|make|cat|ls|cd|mkdir|rm|cp|mv|ps|kill)\b(?:\s+|$)",
        )
        .expect("valid shell cmd regex");

        ClassifierPatterns {
            secret_patterns,
            url_regex,
            code_patterns,
            shell_shebang_or_prompt,
            shell_pipe_or_chain,
            shell_command_prefix,
        }
    })
}

/// Classifica o texto copiado aplicando as regras heurísticas e de segurança.
pub fn classify(input: &str) -> Classification {
    let trimmed = input.trim();
    let p = patterns();

    // Prioridade 1: Segurança (tokens, chaves privadas, senhas explícitas)
    if p.secret_patterns.iter().any(|re| re.is_match(input)) {
        return Classification {
            kind: ContentKind::SecretToken,
            pretty_json: None,
            ttl_seconds: Some(SECRET_TTL_SECONDS),
        };
    }

    // Prioridade 2: JSON válido (objeto ou array) com formatação automática
    if (trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']'))
    {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            return Classification {
                kind: ContentKind::Json,
                pretty_json: serde_json::to_string_pretty(&val).ok(),
                ttl_seconds: None,
            };
        }
    }

    // Prioridade 3: URL em linha única
    if !trimmed.is_empty()
        && !trimmed.chars().any(char::is_whitespace)
        && p.url_regex.is_match(trimmed)
    {
        return Classification {
            kind: ContentKind::Url,
            pretty_json: None,
            ttl_seconds: None,
        };
    }

    // Prioridade 4: ShellCommand vs CodeSnippet
    let first_non_empty_line = trimmed
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");

    if p.shell_shebang_or_prompt.is_match(first_non_empty_line) {
        return Classification {
            kind: ContentKind::ShellCommand,
            pretty_json: None,
            ttl_seconds: None,
        };
    }

    let looks_like_code_syntax = p.code_patterns.iter().any(|re| re.is_match(trimmed))
        || (trimmed.contains('\n')
            && trimmed.contains('{')
            && trimmed.contains('}')
            && trimmed.contains(';'));

    if looks_like_code_syntax {
        return Classification {
            kind: ContentKind::CodeSnippet,
            pretty_json: None,
            ttl_seconds: None,
        };
    }

    let looks_like_shell = (!trimmed.contains('\n') && p.shell_pipe_or_chain.is_match(trimmed))
        || p.shell_command_prefix.is_match(first_non_empty_line)
            && (trimmed.contains(' ') || trimmed.contains('\n') || trimmed.contains('-'));

    if looks_like_shell {
        return Classification {
            kind: ContentKind::ShellCommand,
            pretty_json: None,
            ttl_seconds: None,
        };
    }

    // Fallback: texto simples
    Classification {
        kind: ContentKind::PlainText,
        pretty_json: None,
        ttl_seconds: None,
    }
}

/// Constrói um `ClipEntry` completo já classificado para o timestamp informado.
pub fn build_entry(id: u64, content: &str, now_ts: i64) -> ClipEntry {
    let classification = classify(content);
    let expires_at = classification.ttl_seconds.map(|ttl| now_ts + ttl);

    ClipEntry {
        id,
        content: content.to_string(),
        pretty_json: classification.pretty_json,
        kind: classification.kind,
        created_at: now_ts,
        updated_at: now_ts,
        content_hash: ClipEntry::compute_hash(content),
        use_count: 1,
        pinned: false,
        expires_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_plain_text() {
        let res = classify("Reunião às 15h para revisar a arquitetura do clipvault.");
        assert_eq!(res.kind, ContentKind::PlainText);
        assert_eq!(res.ttl_seconds, None);
        assert_eq!(res.pretty_json, None);
    }

    #[test]
    fn classifies_urls() {
        let res = classify("https://docs.rs/ratatui/latest/ratatui/");
        assert_eq!(res.kind, ContentKind::Url);
        assert_eq!(res.ttl_seconds, None);

        let ssh = classify("ssh://deploy@192.168.1.10/srv/app");
        assert_eq!(ssh.kind, ContentKind::Url);
    }

    #[test]
    fn classifies_valid_json_and_generates_pretty_version() {
        let raw = r#"{"app":"clipvault","features":["fuzzy","ttl"],"active":true}"#;
        let res = classify(raw);
        assert_eq!(res.kind, ContentKind::Json);
        assert_eq!(res.ttl_seconds, None);
        let pretty = res.pretty_json.expect("pretty json should be generated");
        assert!(pretty.contains('\n'));
        assert!(pretty.contains("  \"app\": \"clipvault\""));
    }

    #[test]
    fn classifies_code_snippets() {
        let rust_code = "pub fn sum(a: i32, b: i32) -> i32 {\n    println!(\"{}\", a + b);\n    a + b\n}";
        assert_eq!(classify(rust_code).kind, ContentKind::CodeSnippet);

        let js_code = "const greet = (name) => console.log(`Hi ${name}`);";
        assert_eq!(classify(js_code).kind, ContentKind::CodeSnippet);

        let py_code = "def fibonacci(n):\n    return n if n <= 1 else fibonacci(n-1) + fibonacci(n-2)";
        assert_eq!(classify(py_code).kind, ContentKind::CodeSnippet);
    }

    #[test]
    fn classifies_shell_commands() {
        assert_eq!(
            classify("cargo test --release -- --nocapture").kind,
            ContentKind::ShellCommand
        );
        assert_eq!(
            classify("ps aux | grep clipvault && echo ok").kind,
            ContentKind::ShellCommand
        );
        assert_eq!(
            classify("$ git log --oneline -n 5").kind,
            ContentKind::ShellCommand
        );
        assert_eq!(
            classify("#!/bin/bash\nset -euo pipefail\necho done").kind,
            ContentKind::ShellCommand
        );
    }

    #[test]
    fn classifies_secrets_with_five_minute_ttl_and_masks_preview() {
        let cases = [
            "sk-proj-1234567890abcdef1234567890abcdef",
            "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ123456",
            "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAA=\n-----END OPENSSH PRIVATE KEY-----",
            "DATABASE_PASSWORD=SuperSecretPass123!",
        ];

        for (idx, secret) in cases.iter().enumerate() {
            let now = 1_700_000_000;
            let entry = build_entry(idx as u64 + 1, secret, now);
            assert_eq!(
                entry.kind,
                ContentKind::SecretToken,
                "failed to detect secret for case #{idx}"
            );
            assert_eq!(entry.expires_at, Some(now + SECRET_TTL_SECONDS));
            assert_eq!(entry.expires_at, Some(now + 300));
            assert!(entry.display_preview(false).contains("OCULTOS"));
            assert!(!entry.display_preview(false).contains(secret));
            assert_eq!(entry.display_preview(true), *secret);
        }
    }
}
