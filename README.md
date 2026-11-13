# ClipVault

Gerenciador e **Time-Machine inteligente de clipboard** para Linux (Pop!_OS / X11), escrito em Rust, com classificação semântica automática, mascaramento e auto-expiração de segredos, persistência local embutida e busca *fuzzy* em terminal.

---

## Funcionalidades Principais

- **Monitoramento em Segundo Plano (`daemon`)**: observa alterações no clipboard do sistema via `arboard` com *polling* inteligente (~250ms) e ignora eventos gerados pelo próprio `clipvault` ao restaurar itens antigos.
- **Classificação Heurística Automática**: categoriza cada texto copiado em:
  - `[JSON]` — gera automaticamente uma versão identada (`pretty_json`) para visualização no preview.
  - `[URL]` — links HTTP/HTTPS/FTP/WS/WSS/SSH.
  - `[CODE]` — trechos de código-fonte (Rust, JS/TS, Python, C/C++, etc.).
  - `[CMD]` — comandos de terminal, *pipes* e scripts shell (`#!/bin/bash`, `cargo`, `git`, `docker`, etc.).
  - `[SECRET]` — tokens, chaves de API (`sk-...`, `ghp_...`, `AKIA...`, `AIza...`), `Bearer` tokens, JWTs, chaves privadas PEM/OpenSSH e atribuições explícitas de senha.
  - `[TEXT]` — texto simples (fallback).
- **Segurança por Padrão (`SecretToken`)**:
  - Preview mascarado automaticamente na lista e no painel de detalhes (revelado apenas sob demanda com `s`).
  - **TTL automático de 5 minutos (300 segundos)** com remoção pelo coletor de lixo (*garbage collector*) e nas leituras do banco.
- **Persistência Rápida & Deduplicação**:
  - Banco SQLite embutido via `rusqlite` (`bundled`, sem dependências externas) em modo **WAL** (`journal_mode = WAL`, `busy_timeout = 5000`), permitindo acesso concorrente seguro entre o `daemon` e a `ui`.
  - Deduplicação por hash **SHA-256**: copiar o mesmo conteúdo novamente atualiza o timestamp, incrementa o contador de uso e preserva a fixação (`pinned`).
- **Interface TUI Interativa (`ratatui` + `crossterm`)**:
  - Busca *fuzzy* em tempo real (`fuzzy-matcher`), painel dividido entre histórico e preview formatado.

---

## Arquitetura (`src/`)

- `src/domain.rs` — Modelos centrais (`ClipEntry`, `ContentKind`), cálculo de SHA-256, formatação de preview seguro e tempo relativo.
- `src/classifier.rs` — Motor heurístico baseado em `regex` e `serde_json` com priorização de segurança para `SecretToken`.
- `src/storage.rs` — Persistência local embutida com `rusqlite` (SQLite em modo WAL), CRUD, deduplicação por hash, controle de `last_restored_hash`, limpeza de expirados e busca *fuzzy*.
- `src/watcher.rs` — Thread de monitoramento do clipboard via `arboard` com canal `std::sync::mpsc`, filtro anti-eco e GC periódico.
- `src/tui.rs` — Interface interativa de terminal com `ratatui` + `crossterm`.
- `src/main.rs` — CLI via `clap` com suporte aos subcomandos `daemon` e `ui` (padrão).

---

## Instalação no Linux (Pop!_OS / X11)

### Pré-requisitos

- Toolchain Rust (`cargo` e `rustc`) instalado via [rustup](https://rustup.rs/).
- Sessão X11 com bibliotecas XCB padrão (já presentes no Pop!_OS; caso necessário em instalações mínimas: `sudo apt install libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev`).

### Compilação e Instalação

```bash
cargo build --release
cargo install --path .
```

O binário otimizado também fica disponível diretamente em `./target/release/clipvault`.

---

## Uso

### 1. Iniciar o monitor em segundo plano (`daemon`)

```bash
clipvault daemon
```

O banco local padrão é criado automaticamente em `~/.local/share/clipvault/clipvault.db`. Você pode informar outro arquivo com `--db`:

```bash
clipvault --db /tmp/meu-historico.db daemon
```

### 2. Abrir a interface interativa (`ui`)

```bash
clipvault
# ou explicitamente:
clipvault ui
```

---

## Atalhos da TUI

| Atalho | Ação |
| --- | --- |
| `Enter` | Copia o item selecionado de volta para o clipboard e sai da TUI |
| `↑` / `↓` ou `j` / `k` | Navega pelo histórico (oculta segredos novamente ao trocar de item) |
| `/` ou `Tab` | Alterna o foco para digitar na barra de busca *fuzzy* em tempo real |
| `p` (ou `Ctrl+P`) | Fixa ou desafixa o item selecionado no topo da lista |
| `d` (ou `Ctrl+D` / `Delete`) | Deleta o item selecionado do histórico |
| `s` (ou `Ctrl+S`) | Revela ou oculta o conteúdo de um `SecretToken` no painel de preview |
| `Backspace` | Remove o último caractere do filtro de busca |
| `Esc` | Limpa a busca ativa ou encerra a TUI |

---

## Testes

Execute toda a suíte de testes unitários com:

```bash
cargo test
```
