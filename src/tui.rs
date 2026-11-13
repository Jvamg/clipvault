use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{FilterType, Resize, StatefulImage};

use crate::domain::{ClipEntry, ContentKind};
use crate::storage::Storage;

const TICK_RATE: Duration = Duration::from_millis(250);
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// Determina se o terminal suporta protocolo Kitty quando a detecção automática cai em Halfblocks.
pub fn should_use_kitty_protocol(
    guessed: ProtocolType,
    term_program: Option<&str>,
    term: Option<&str>,
) -> bool {
    if guessed != ProtocolType::Halfblocks {
        return false;
    }
    let matches_kitty_term = |val: &str| {
        let lower = val.to_ascii_lowercase();
        lower.contains("kitty") || lower.contains("ghostty") || lower.contains("wezterm")
    };
    term_program.is_some_and(matches_kitty_term) || term.is_some_and(matches_kitty_term)
}

/// Estado completo da interface interativa de terminal.
pub struct App {
    pub storage: Arc<Storage>,
    pub query: String,
    pub search_focused: bool,
    pub entries: Vec<ClipEntry>,
    pub list_state: ListState,
    pub reveal_secret: bool,
    pub should_quit: bool,
    pub status_message: Option<String>,
    pub picker: Picker,
    pub image_preview_state: Option<(u64, Box<dyn StatefulProtocol>)>,
}

impl App {
    pub fn new(storage: Arc<Storage>) -> Result<Self> {
        let mut app = Self {
            storage,
            query: String::new(),
            search_focused: false,
            entries: Vec::new(),
            list_state: ListState::default(),
            reveal_secret: false,
            should_quit: false,
            status_message: None,
            picker: Picker::new((8, 16)),
            image_preview_state: None,
        };
        app.refresh_entries(None)?;
        Ok(app)
    }

    /// Detecta o protocolo gráfico do terminal (Kitty/Sixel/Halfblocks) após entrar na tela alternativa.
    pub fn init_terminal_picker(&mut self) {
        let mut picker = Picker::from_termios().unwrap_or_else(|_| Picker::new((9, 18)));
        let guessed = picker.guess_protocol();
        let term_program = std::env::var("TERM_PROGRAM").ok();
        let term = std::env::var("TERM").ok();

        if should_use_kitty_protocol(guessed, term_program.as_deref(), term.as_deref()) {
            picker.protocol_type = ProtocolType::Kitty;
        }

        self.picker = picker;
    }

    /// Garante que o protocolo de renderização da imagem selecionada esteja pronto para o painel.
    pub fn ensure_image_protocol(&mut self) {
        let Some(entry) = self.selected_entry() else {
            self.image_preview_state = None;
            return;
        };

        if entry.kind != ContentKind::Image {
            self.image_preview_state = None;
            return;
        }

        let entry_id = entry.id;
        if self
            .image_preview_state
            .as_ref()
            .is_some_and(|(cached_id, _)| *cached_id == entry_id)
        {
            return;
        }

        if let Some(blob) = entry.image_blob.as_deref() {
            if let Ok(dyn_img) = image::load_from_memory(blob) {
                let proto = self.picker.new_resize_protocol(dyn_img);
                self.image_preview_state = Some((entry_id, proto));
                return;
            }
        }

        self.image_preview_state = None;
    }

    pub fn selected_entry(&self) -> Option<&ClipEntry> {
        self.list_state
            .selected()
            .and_then(|idx| self.entries.get(idx))
    }

    pub fn refresh_entries(&mut self, preserve_id: Option<u64>) -> Result<()> {
        let now_ts = Utc::now().timestamp();
        let prev_idx = self.list_state.selected();

        self.entries = if self.query.trim().is_empty() {
            self.storage.list_all(now_ts)?
        } else {
            self.storage.search(&self.query, now_ts)?
        };

        if self.entries.is_empty() {
            self.list_state.select(None);
            self.reveal_secret = false;
            return Ok(());
        }

        let next_idx = if let Some(target_id) = preserve_id {
            self.entries
                .iter()
                .position(|entry| entry.id == target_id)
                .unwrap_or(0)
        } else if let Some(idx) = prev_idx {
            idx.min(self.entries.len() - 1)
        } else {
            0
        };

        self.list_state.select(Some(next_idx));
        Ok(())
    }

    pub fn select_next(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let next = match self.list_state.selected() {
            Some(idx) if idx + 1 < self.entries.len() => idx + 1,
            Some(idx) => idx,
            None => 0,
        };
        if self.list_state.selected() != Some(next) {
            self.reveal_secret = false;
        }
        self.list_state.select(Some(next));
    }

    pub fn select_previous(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let prev = match self.list_state.selected() {
            Some(idx) => idx.saturating_sub(1),
            None => 0,
        };
        if self.list_state.selected() != Some(prev) {
            self.reveal_secret = false;
        }
        self.list_state.select(Some(prev));
    }

    pub fn toggle_pin_selected(&mut self) -> Result<()> {
        let Some(entry) = self.selected_entry().cloned() else {
            return Ok(());
        };
        if let Some(updated) = self.storage.toggle_pin(entry.id)? {
            self.status_message = Some(if updated.pinned {
                format!("Item #{} fixado no topo.", updated.id)
            } else {
                format!("Item #{} desafixado.", updated.id)
            });
            self.refresh_entries(Some(updated.id))?;
        }
        Ok(())
    }

    pub fn delete_selected(&mut self) -> Result<()> {
        let Some(entry) = self.selected_entry().cloned() else {
            return Ok(());
        };
        let current_idx = self.list_state.selected();
        if self.storage.delete(entry.id)? {
            self.reveal_secret = false;
            self.status_message = Some(format!("Item #{} removido do histórico.", entry.id));
            self.refresh_entries(None)?;
            if !self.entries.is_empty() {
                if let Some(idx) = current_idx {
                    self.list_state
                        .select(Some(idx.min(self.entries.len() - 1)));
                }
            }
        }
        Ok(())
    }

    pub fn toggle_reveal_secret(&mut self) {
        self.reveal_secret = !self.reveal_secret;
        self.status_message = Some(if self.reveal_secret {
            "Preview de segredo revelado.".to_string()
        } else {
            "Preview de segredo ocultado.".to_string()
        });
    }

    pub fn restore_selected_to_clipboard(&mut self) -> Result<()> {
        let Some(entry) = self.selected_entry().cloned() else {
            self.should_quit = true;
            return Ok(());
        };

        let now_ts = Utc::now().timestamp();
        if entry.is_expired(now_ts) {
            self.status_message = Some("Este segredo já expirou e foi removido.".to_string());
            self.refresh_entries(None)?;
            return Ok(());
        }

        // Registra o hash antes de copiar para que o daemon em execução ignore o eco.
        let _ = self.storage.record_restore(entry.id, now_ts)?;

        if entry.kind == ContentKind::Image {
            let blob = entry
                .image_blob
                .as_deref()
                .context("registro de imagem não possui dados PNG armazenados")?;
            crate::watcher::set_clipboard_image(blob)
                .context("falha ao copiar imagem selecionada para o clipboard")?;
        } else {
            crate::watcher::set_clipboard_text(&entry.content)
                .context("falha ao copiar item selecionado para o clipboard")?;
        }

        self.should_quit = true;
        Ok(())
    }

    pub fn handle_key_event(&mut self, key: KeyEvent) -> Result<()> {
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        match key.code {
            KeyCode::Esc => {
                if !self.query.is_empty() {
                    self.query.clear();
                    self.search_focused = false;
                    self.reveal_secret = false;
                    self.refresh_entries(None)?;
                } else {
                    self.should_quit = true;
                }
            }
            KeyCode::Enter => {
                self.restore_selected_to_clipboard()?;
            }
            KeyCode::Up => self.select_previous(),
            KeyCode::Down => self.select_next(),
            KeyCode::Delete => self.delete_selected()?,
            KeyCode::Tab => {
                self.search_focused = !self.search_focused;
            }
            KeyCode::Backspace => {
                if !self.query.is_empty() {
                    self.query.pop();
                    self.reveal_secret = false;
                    self.refresh_entries(None)?;
                }
            }
            KeyCode::Char('p') if ctrl || !self.search_focused => {
                self.toggle_pin_selected()?;
            }
            KeyCode::Char('d') if ctrl || !self.search_focused => {
                self.delete_selected()?;
            }
            KeyCode::Char('s') if ctrl || !self.search_focused => {
                self.toggle_reveal_secret();
            }
            KeyCode::Char('j') if !self.search_focused && !ctrl => {
                self.select_next();
            }
            KeyCode::Char('k') if !self.search_focused && !ctrl => {
                self.select_previous();
            }
            KeyCode::Char('/') if !self.search_focused && !ctrl => {
                self.search_focused = true;
            }
            KeyCode::Char('c') if ctrl => {
                self.should_quit = true;
            }
            KeyCode::Char(ch) if self.search_focused && !ctrl => {
                self.query.push(ch);
                self.reveal_secret = false;
                self.refresh_entries(None)?;
            }
            _ => {}
        }

        Ok(())
    }
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn init() -> Result<Self> {
        enable_raw_mode().context("falha ao habilitar raw mode no terminal")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)
            .context("falha ao entrar na tela alternativa do terminal")?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend).context("falha ao inicializar ratatui")?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

/// Inicia o loop principal da interface TUI.
pub fn run(storage: Arc<Storage>) -> Result<()> {
    let mut app = App::new(storage)?;
    let mut guard = TerminalGuard::init()?;
    app.init_terminal_picker();
    let mut last_refresh = Instant::now();

    while !app.should_quit {
        guard.terminal.draw(|frame| draw_ui(frame, &mut app))?;

        if event::poll(TICK_RATE)? {
            if let Event::Key(key) = event::read()? {
                app.handle_key_event(key)?;
            }
        }

        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            let selected_id = app.selected_entry().map(|e| e.id);
            app.refresh_entries(selected_id)?;
            last_refresh = Instant::now();
        }
    }

    Ok(())
}

fn kind_style(kind: ContentKind) -> Style {
    match kind {
        ContentKind::SecretToken => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ContentKind::Json => Style::default().fg(Color::Yellow),
        ContentKind::Url => Style::default().fg(Color::Cyan),
        ContentKind::CodeSnippet => Style::default().fg(Color::Green),
        ContentKind::ShellCommand => Style::default().fg(Color::Magenta),
        ContentKind::PlainText => Style::default().fg(Color::Gray),
        ContentKind::Image => Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
    }
}

fn draw_ui(frame: &mut Frame, app: &mut App) {
    let now_ts = Utc::now().timestamp();

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(3),
        ])
        .split(frame.area());

    // 1. Barra de busca fuzzy no topo
    let search_border_style = if app.search_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let search_mode_hint = if app.search_focused {
        "DIGITANDO (Tab/Esc p/ sair)"
    } else {
        "NAVEGAÇÃO (pressione '/' ou Tab p/ buscar)"
    };
    let search_title = format!(
        " ClipVault Time-Machine | Busca Fuzzy ({} itens) [{}] ",
        app.entries.len(),
        search_mode_hint
    );
    let search_text = if app.query.is_empty() && !app.search_focused {
        Span::styled(
            "Pressione '/' para filtrar o histórico...",
            Style::default().fg(Color::DarkGray),
        )
    } else {
        Span::raw(format!("> {}", app.query))
    };
    let search_bar = Paragraph::new(Line::from(search_text)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(search_border_style)
            .title(search_title),
    );
    frame.render_widget(search_bar, vertical[0]);

    // 2. Painel central dividido em lista (esquerda) e preview (direita)
    let center = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(vertical[1]);

    let items: Vec<ListItem> = app
        .entries
        .iter()
        .map(|entry| {
            let pin_prefix = if entry.pinned { "📌 " } else { "   " };
            let rel_time = entry.relative_time(now_ts);
            let summary = entry.single_line_summary(false, 42);
            let line = Line::from(vec![
                Span::styled(pin_prefix, Style::default().fg(Color::Yellow)),
                Span::styled(format!("{:<8}", entry.kind.tag()), kind_style(entry.kind)),
                Span::raw(" "),
                Span::styled(format!("{:<7}", rel_time), Style::default().fg(Color::DarkGray)),
                Span::raw(" "),
                Span::raw(summary),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Histórico "),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    frame.render_stateful_widget(list, center[0], &mut app.list_state);

    if let Some(entry) = app.selected_entry().cloned() {
        if entry.kind == ContentKind::Image {
            let outer_block = Block::default()
                .borders(Borders::ALL)
                .title(" Preview & Metadados ");
            let inner_area = outer_block.inner(center[1]);
            frame.render_widget(outer_block, center[1]);

            let preview_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(3), Constraint::Min(4)])
                .split(inner_area);

            let (w, h) = entry.image_dimensions.unwrap_or((0, 0));
            let size_bytes = entry.image_blob.as_ref().map_or(0, Vec::len);
            let kb = size_bytes.div_ceil(1024).max(1);
            let formatted_date = DateTime::from_timestamp(entry.updated_at, 0)
                .map(|dt| {
                    dt.with_timezone(&Local)
                        .format("%d/%m/%Y %H:%M:%S")
                        .to_string()
                })
                .unwrap_or_else(|| entry.updated_at.to_string());
            let short_hash: String = entry.content_hash.chars().take(12).collect();

            let meta_lines = vec![
                Line::from(vec![
                    Span::styled(
                        format!("#{} ", entry.id),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(entry.kind.tag(), kind_style(entry.kind)),
                    Span::styled(
                        format!(
                            " | {w}x{h} px | {kb} KB | Usos: {} | Fixado: {}",
                            entry.use_count,
                            if entry.pinned { "Sim" } else { "Não" },
                        ),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]),
                Line::from(Span::styled(
                    format!("Data: {formatted_date} | SHA: {short_hash}"),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    "─".repeat(inner_area.width as usize),
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            frame.render_widget(Paragraph::new(meta_lines), preview_chunks[0]);

            app.ensure_image_protocol();
            if let Some((_, proto)) = app.image_preview_state.as_mut() {
                let image_widget =
                    StatefulImage::new(None).resize(Resize::Fit(Some(FilterType::CatmullRom)));
                frame.render_stateful_widget(image_widget, preview_chunks[1], proto);
            } else {
                frame.render_widget(
                    Paragraph::new("Não foi possível decodificar o preview da imagem.")
                        .style(Style::default().fg(Color::Red)),
                    preview_chunks[1],
                );
            }
        } else {
            app.image_preview_state = None;
            let mut lines = Vec::new();
            let ttl_info = match entry.expires_at {
                Some(exp) => {
                    let rem = (exp - now_ts).max(0);
                    format!(" | Expira em {}m{:02}s", rem / 60, rem % 60)
                }
                None => String::new(),
            };
            let short_hash: String = entry.content_hash.chars().take(12).collect();
            lines.push(Line::from(vec![
                Span::styled(
                    format!("#{} ", entry.id),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(entry.kind.tag(), kind_style(entry.kind)),
                Span::styled(
                    format!(
                        " | Usos: {} | Fixado: {} | SHA: {}{}",
                        entry.use_count,
                        if entry.pinned { "Sim" } else { "Não" },
                        short_hash,
                        ttl_info
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
            lines.push(Line::from(Span::styled(
                "─".repeat(48),
                Style::default().fg(Color::DarkGray),
            )));

            let body = entry.display_preview(app.reveal_secret);
            let body_style = if entry.kind.is_secret() && !app.reveal_secret {
                Style::default().fg(Color::Red)
            } else {
                Style::default()
            };
            for raw_line in body.lines() {
                lines.push(Line::from(Span::styled(raw_line.to_string(), body_style)));
            }

            let preview_widget = Paragraph::new(lines)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Preview & Metadados "),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(preview_widget, center[1]);
        }
    } else {
        app.image_preview_state = None;
        let preview_widget = Paragraph::new("Nenhum item encontrado no histórico.")
            .style(Style::default().fg(Color::DarkGray))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Preview & Metadados "),
            );
        frame.render_widget(preview_widget, center[1]);
    }

    // 3. Barra de atalhos / status no rodapé
    let shortcuts =
        "Enter: Copiar e Sair | p: Fixar | d: Deletar | s: Revelar/Ocultar Segredo | /: Buscar | Esc: Limpar/Sair";
    let footer_line = if let Some(msg) = &app.status_message {
        Line::from(vec![
            Span::styled(format!("{msg}  |  "), Style::default().fg(Color::Green)),
            Span::styled(shortcuts, Style::default().fg(Color::Gray)),
        ])
    } else {
        Line::from(Span::styled(shortcuts, Style::default().fg(Color::Gray)))
    };

    let footer = Paragraph::new(footer_line)
        .block(Block::default().borders(Borders::ALL).title(" Atalhos "));
    frame.render_widget(footer, vertical[2]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_app() -> (tempfile::TempDir, App) {
        let dir = tempdir().expect("tempdir");
        let storage = Arc::new(Storage::open(dir.path().join("tui_test.db")).expect("open"));
        let now = Utc::now().timestamp();
        storage
            .insert_or_update("primeiro item normal", now - 20)
            .expect("insert 1");
        storage
            .insert_or_update("sk-secret1234567890abcdef12", now - 10)
            .expect("insert 2");
        storage
            .insert_or_update(r#"{"hello":"world"}"#, now)
            .expect("insert 3");
        let app = App::new(storage).expect("app");
        (dir, app)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn navigation_resets_secret_reveal_and_toggles_pin_and_delete() {
        let (_dir, mut app) = test_app();
        assert_eq!(app.entries.len(), 3);
        assert_eq!(app.selected_entry().expect("first").kind, ContentKind::Json);

        // Navega para o SecretToken e revela
        app.handle_key_event(key(KeyCode::Down)).expect("down");
        assert_eq!(
            app.selected_entry().expect("second").kind,
            ContentKind::SecretToken
        );
        app.handle_key_event(key(KeyCode::Char('s'))).expect("reveal");
        assert!(app.reveal_secret);

        // Ao trocar de item, `reveal_secret` deve voltar para false por segurança
        app.handle_key_event(key(KeyCode::Down)).expect("down 2");
        assert!(!app.reveal_secret);

        // Fixa o terceiro item e verifica que subiu para o topo
        let third_id = app.selected_entry().expect("third").id;
        app.handle_key_event(key(KeyCode::Char('p'))).expect("pin");
        assert_eq!(app.entries[0].id, third_id);
        assert!(app.entries[0].pinned);

        // Deleta o item selecionado e verifica que restam 2
        app.handle_key_event(key(KeyCode::Char('d'))).expect("delete");
        assert_eq!(app.entries.len(), 2);
    }

    #[test]
    fn search_mode_filters_and_esc_clears_before_quitting() {
        let (_dir, mut app) = test_app();

        app.handle_key_event(key(KeyCode::Char('/'))).expect("focus search");
        assert!(app.search_focused);

        for ch in "hello".chars() {
            app.handle_key_event(key(KeyCode::Char(ch))).expect("type char");
        }
        assert_eq!(app.query, "hello");
        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.entries[0].kind, ContentKind::Json);

        // Primeiro Esc limpa a busca sem fechar a TUI
        app.handle_key_event(key(KeyCode::Esc)).expect("esc clear");
        assert!(app.query.is_empty());
        assert!(!app.should_quit);
        assert_eq!(app.entries.len(), 3);

        // Segundo Esc encerra a TUI
        app.handle_key_event(key(KeyCode::Esc)).expect("esc quit");
        assert!(app.should_quit);
    }

    #[test]
    fn image_entry_renders_preview_with_test_backend() {
        let dir = tempdir().expect("tempdir");
        let storage = Arc::new(Storage::open(dir.path().join("tui_img.db")).expect("open"));
        let png = crate::watcher::encode_rgba_to_png(4, 4, &[200; 64]).expect("png");
        let now = Utc::now().timestamp();
        let entry = storage
            .insert_or_update_image(png, 4, 4, now)
            .expect("insert image")
            .expect("entry");

        let mut app = App::new(storage).expect("app");
        assert_eq!(app.selected_entry().expect("img").id, entry.id);

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw_ui(frame, &mut app))
            .expect("draw image ui");

        assert!(app
            .image_preview_state
            .as_ref()
            .is_some_and(|(id, _)| *id == entry.id));
    }

    #[test]
    fn kitty_protocol_fallback_detects_modern_terminals() {
        assert!(!should_use_kitty_protocol(
            ProtocolType::Halfblocks,
            Some("cosmic-term"),
            Some("xterm-256color"),
        ));
        assert!(should_use_kitty_protocol(
            ProtocolType::Halfblocks,
            None,
            Some("xterm-kitty"),
        ));
        assert!(should_use_kitty_protocol(
            ProtocolType::Halfblocks,
            Some("ghostty"),
            None,
        ));
        assert!(should_use_kitty_protocol(
            ProtocolType::Halfblocks,
            Some("WezTerm"),
            Some("xterm-256color"),
        ));
        assert!(!should_use_kitty_protocol(
            ProtocolType::Halfblocks,
            Some("gnome-terminal"),
            Some("xterm-256color"),
        ));
        assert!(!should_use_kitty_protocol(
            ProtocolType::Sixel,
            Some("kitty"),
            None,
        ));
    }
}
