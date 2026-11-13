# ClipVault

Gerenciador e **Time-Machine inteligente de clipboard** escrito em Rust, com classificação semântica automática, captura e preview gráfico de imagens no terminal, mascaramento e auto-expiração de segredos, persistência local embutida e busca *fuzzy*.

## Compatibilidade

O `clipvault` funciona em **qualquer distribuição Linux (Wayland e X11)** — incluindo Pop!_OS, Ubuntu, Debian, Fedora, Arch Linux e derivados — além de compilar nativamente em **macOS e Windows**, utilizando `arboard` e SQLite embutido via `rusqlite` (`bundled`, sem dependências externas em tempo de execução).

- **Linux (Wayland)**: suporte nativo à persistência do clipboard após fechar a TUI e captura de texto/imagens PNG (recomendado instalar `wl-clipboard`, que fornece `wl-paste` e `wl-copy`).
- **Linux (X11)**: suporte via `arboard` e fallback automático para `xclip` ou `xsel`.
- **macOS e Windows**: compilação direta via `cargo build --release` com captura/restauração via `arboard`.

---

## Funcionalidades Principais

- **Monitoramento em Segundo Plano (`daemon`)**: observa alterações na área de transferência (~250ms), suporta texto e imagens PNG e ignora ecos gerados pelo próprio `clipvault` ao restaurar itens antigos.
- **Classificação Heurística Automática**: categoriza cada item copiado em:
  - `[JSON]` — gera automaticamente uma versão indentada (`pretty_json`) para visualização no preview.
  - `[URL]` — links `http`/`https`/`ftp`/`ws`/`wss`/`ssh`.
  - `[CODE]` — trechos de código-fonte (Rust, JS/TS, Python, C/C++, etc.).
  - `[CMD]` — comandos de terminal, *pipes* e scripts shell (`#!/bin/bash`, `cargo`, `git`, `docker`, etc.).
  - `[SECRET]` — tokens, chaves de API (`sk-...`, `ghp_...`, `AKIA...`, `AIza...`), `Bearer` tokens, JWTs, chaves privadas PEM/OpenSSH e atribuições explícitas de senha.
  - `[IMG]` — capturas de tela e imagens PNG copiadas (com preview gráfico no terminal via Sixel/Kitty/Halfblocks, restauração via `Enter` e abertura externa em resolução original 1:1 com `v`).
  - `[TEXT]` — texto simples (fallback).
- **Segurança por Padrão (`SecretToken`)**:
  - Preview mascarado automaticamente na lista e no painel de detalhes (revelado apenas sob demanda com `s`).
  - **TTL automático de 5 minutos (300 segundos)** com limpeza automática.
- **Persistência Rápida & Deduplicação**:
  - Banco SQLite embutido em modo **WAL** (`journal_mode = WAL`, `busy_timeout = 5000`), permitindo acesso concorrente seguro entre o `daemon` e a `ui`.
  - Deduplicação por hash **SHA-256**: copiar o mesmo conteúdo novamente atualiza o timestamp, incrementa o contador de uso e preserva a fixação (`pinned`).
- **Interface TUI Interativa (`ratatui` + `crossterm`)**:
  - Busca *fuzzy* em tempo real (`fuzzy-matcher`), painel dividido entre histórico e preview formatado/visual.

---

## Instalação e Daemon Automático (`systemd`)

### 1. Pré-requisitos

- Toolchain Rust (`cargo` e `rustc`) instalada via [rustup](https://rustup.rs/).
- No Linux, ferramentas recomendadas de integração de clipboard para o ambiente gráfico em uso:
  - **Wayland**: `wl-clipboard` (`sudo apt install wl-clipboard` / `sudo dnf install wl-clipboard` / `sudo pacman -S wl-clipboard`)
  - **X11**: `xclip` ou `xsel` e bibliotecas XCB padrão (caso necessário em instalações mínimas Debian/Ubuntu: `sudo apt install xclip libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev`)

### 2. Clonar, Compilar e Instalar em `~/.local/bin`

```bash
git clone [https://github.com/Jvamg/clipvault.git](https://github.com/Jvamg/clipvault.git)
cd clipvault
cargo build --release
mkdir -p ~/.local/bin
cp target/release/clipvault ~/.local/bin/clipvault
```

> Certifique-se de que `~/.local/bin` esteja no seu `$PATH` (ou utilize o caminho completo `~/.local/bin/clipvault`).

### 3. Configurar o Daemon Automático (`systemd --user`)

Crie o arquivo de serviço `~/.config/systemd/user/clipvault.service` para que o monitoramento em segundo plano inicie automaticamente ao fazer login:

```bash
mkdir -p ~/.config/systemd/user
cat << 'EOF' > ~/.config/systemd/user/clipvault.service
[Unit]
Description=ClipVault Clipboard Time-Machine Daemon
After=graphical-session.target

[Service]
ExecStart=%h/.local/bin/clipvault daemon
Restart=always
RestartSec=2

[Install]
WantedBy=default.target
EOF
```

Ative e inicie o serviço imediatamente:

```bash
systemctl --user daemon-reload
systemctl --user enable --now clipvault.service
systemctl --user status clipvault.service
```

Para acompanhar os logs do daemon em tempo real:

```bash
journalctl --user -u clipvault.service -f
```

---

## Configurando o Atalho Global (`Super + V` / `Win + V`)

Para abrir o **ClipVault** rapidamente de qualquer lugar pressionando `Super + V` (tecla Windows/Super + `V`), cadastre um atalho personalizado nas configurações do seu ambiente de desktop apontando para o seu emulador de terminal preferido:

1. Abra as **Configurações do Sistema** → **Teclado** → **Atalhos Personalizados** (ou seção equivalente do seu gerenciador de janelas).
2. Adicione um novo atalho:
   - **Nome**: `ClipVault`
   - **Combinação de teclas**: `Super + V`
   - **Comando** (escolha o correspondente ao seu terminal):

- **Pop!_OS / COSMIC Terminal**:
  ```bash
  cosmic-term -e ~/.local/bin/clipvault
  ```
- **GNOME Terminal (Ubuntu / Fedora / Debian)**:
  ```bash
  gnome-terminal -- ~/.local/bin/clipvault
  ```
- **Kitty**:
  ```bash
  kitty ~/.local/bin/clipvault
  ```
- **Alacritty**:
  ```bash
  alacritty -e ~/.local/bin/clipvault
  ```

> **Dica:** Em ambientes com Wayland/COSMIC, você também pode usar `sh -c '~/.local/bin/clipvault'` caso seu gerenciador de janelas não expanda `~` automaticamente — nesse caso, substitua `~/.local/bin/clipvault` pelo caminho absoluto (ex.: `/home/seu-usuario/.local/bin/clipvault`).

---

## Uso Manual (CLI)

### 1. Iniciar o daemon manualmente

```bash
clipvault daemon
```

O banco local padrão é criado automaticamente em `~/.local/share/clipvault/clipvault.db`. Você pode informar outro caminho com `--db`:

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
| `Enter` | Copia o item selecionado (texto ou imagem `[IMG]`) de volta para o clipboard e sai da TUI |
| `v` (ou `Ctrl+V`) | Salva a imagem selecionada (`[IMG]`) em `/tmp/clipvault-preview.png` e abre no visualizador padrão (`xdg-open`) |
| `↑` / `↓` ou `j` / `k` | Navega pelo histórico (oculta segredos novamente ao trocar de item) |
| `/` ou `Tab` | Alterna o foco para digitar na barra de busca *fuzzy* em tempo real |
| `p` (ou `Ctrl+P`) | Fixa ou desafixa o item selecionado no topo da lista |
| `d` (ou `Ctrl+D` / `Delete`) | Deleta o item selecionado do histórico |
| `s` (ou `Ctrl+S`) | Revela ou oculta o conteúdo de um `SecretToken` no painel de preview |
| `Backspace` | Remove o último caractere do filtro de busca |
| `Esc` | Limpa a busca ativa ou encerra a TUI |

---

## Arquitetura (`src/`)

- `src/domain.rs` — Modelos centrais (`ClipEntry`, `ContentKind`), cálculo de SHA-256, formatação de preview seguro e tempo relativo.
- `src/classifier.rs` — Motor heurístico baseado em `regex` e `serde_json` com priorização de segurança para `SecretToken`.
- `src/storage.rs` — Persistência local embutida com `rusqlite` (SQLite em modo WAL), CRUD, deduplicação por hash, armazenamento de blobs PNG, controle de `last_restored_hash`, limpeza de expirados e busca *fuzzy*.
- `src/watcher.rs` — Thread de monitoramento do clipboard via `arboard` + suporte Wayland/X11 (`wl-paste`/`wl-copy`, `xclip`/`xsel`) com canal `std::sync::mpsc`, filtro anti-eco e GC periódico.
- `src/tui.rs` — Interface interativa de terminal com `ratatui`, `crossterm` e `ratatui-image` (Sixel/Kitty/Halfblocks).
- `src/main.rs` — CLI via `clap` com suporte aos subcomandos `daemon` e `ui` (padrão).

---

## Testes

Execute toda a suíte de testes unitários com:

```bash
cargo test
```
