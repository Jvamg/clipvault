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
pub const PREVIEW_IMAGE_PATH: &str = "/tmp/clipvault-preview.png";

/// Resolves the graphics protocol when `guess_protocol()` falls back to `Halfblocks`:
/// - `ProtocolType::Sixel` on COSMIC Terminal (`TERM_PROGRAM` contains `cosmic` or `COSMIC_VERSION` is set);
/// - `ProtocolType::Kitty` on `kitty`, `ghostty`, or `wezterm`.
pub fn resolve_protocol_type(
    guessed: ProtocolType,
    term_program: Option<&str>,
    term: Option<&str>,
    cosmic_version: Option<&str>,
) -> ProtocolType {
    if guessed != ProtocolType::Halfblocks {
        return guessed;
    }
    let is_cosmic = term_program
        .is_some_and(|val| val.to_ascii_lowercase().contains("cosmic"))
        || cosmic_version.is_some();
    if is_cosmic {
        return ProtocolType::Sixel;
    }
    let matches_kitty_term = |val: &str| {
        let lower = val.to_ascii_lowercase();
        lower.contains("kitty") || lower.contains("ghostty") || lower.contains("wezterm")
    };
    if term_program.is_some_and(matches_kitty_term) || term.is_some_and(matches_kitty_term) {
        return ProtocolType::Kitty;
    }
    ProtocolType::Halfblocks
}

#[cfg(not(test))]
fn spawn_external_viewer(path: &str) -> io::Result<()> {
    use std::process::{Command, Stdio};

    Command::new("xdg-open")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
fn spawn_external_viewer(_path: &str) -> io::Result<()> {
    Ok(())
}

/// Complete state of the interactive terminal interface.
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

    /// Detects the terminal graphics protocol (Kitty/Sixel/Halfblocks) after entering the alternate screen.
    pub fn init_terminal_picker(&mut self) {
        let mut picker = Picker::from_termios().unwrap_or_else(|_| Picker::new((9, 18)));
        let guessed = picker.guess_protocol();
        let term_program = std::env::var("TERM_PROGRAM").ok();
        let term = std::env::var("TERM").ok();
        let cosmic_version = std::env::var("COSMIC_VERSION").ok();

        picker.protocol_type = resolve_protocol_type(
            guessed,
            term_program.as_deref(),
            term.as_deref(),
            cosmic_version.as_deref(),
        );

        self.picker = picker;
    }

    /// Ensures the rendering protocol state is ready for the currently selected image entry.
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
                format!("Item #{} pinned to top.", updated.id)
            } else {
                format!("Item #{} unpinned.", updated.id)
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
            self.status_message = Some(format!("Item #{} removed from history.", entry.id));
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
            "Secret preview revealed.".to_string()
        } else {
            "Secret preview hidden.".to_string()
        });
    }

    pub fn open_selected_image_external(&mut self) -> Result<()> {
        let Some(entry) = self.selected_entry() else {
            return Ok(());
        };
        if entry.kind != ContentKind::Image {
            self.status_message = Some(
                "External viewer ('v') is only available for [IMG] entries.".to_string(),
            );
            return Ok(());
        }
        let blob = entry
            .image_blob
            .as_deref()
            .context("image entry has no stored PNG data")?;
        std::fs::write(PREVIEW_IMAGE_PATH, blob)
            .with_context(|| format!("failed to save image to {PREVIEW_IMAGE_PATH}"))?;

        match spawn_external_viewer(PREVIEW_IMAGE_PATH) {
            Ok(()) => {
                self.status_message = Some(format!(
                    "Opening image at {PREVIEW_IMAGE_PATH} in default viewer..."
                ));
            }
            Err(err) => {
                self.status_message = Some(format!(
                    "PNG saved to {PREVIEW_IMAGE_PATH} (failed to launch xdg-open: {err})"
                ));
            }
        }
        Ok(())
    }

    pub fn restore_selected_to_clipboard(&mut self) -> Result<()> {
        let Some(entry) = self.selected_entry().cloned() else {
            self.should_quit = true;
            return Ok(());
        };

        let now_ts = Utc::now().timestamp();
        if entry.is_expired(now_ts) {
            self.status_message =
                Some("This secret has already expired and was removed.".to_string());
            self.refresh_entries(None)?;
            return Ok(());
        }

        // Record the hash before copying so the running daemon ignores the self-triggered echo.
        let _ = self.storage.record_restore(entry.id, now_ts)?;

        if entry.kind == ContentKind::Image {
            let blob = entry
                .image_blob
                .as_deref()
                .context("image entry has no stored PNG data")?;
            crate::watcher::set_clipboard_image(blob)
                .context("failed to copy selected image to clipboard")?;
        } else {
            crate::watcher::set_clipboard_text(&entry.content)
                .context("failed to copy selected entry to clipboard")?;
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
            KeyCode::Char('v') if ctrl || !self.search_focused => {
                self.open_selected_image_external()?;
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
        enable_raw_mode().context("failed to enable terminal raw mode")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)
            .context("failed to enter terminal alternate screen")?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend).context("failed to initialize ratatui")?;
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

/// Starts the main interactive TUI event loop.
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

    // 1. Top fuzzy search bar
    let search_border_style = if app.search_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let search_mode_hint = if app.search_focused {
        "TYPING (Tab/Esc to exit)"
    } else {
        "NAVIGATION (press '/' or Tab to search)"
    };
    let search_title = format!(
        " ClipVault Time-Machine | Fuzzy Search ({} items) [{}] ",
        app.entries.len(),
        search_mode_hint
    );
    let search_text = if app.query.is_empty() && !app.search_focused {
        Span::styled(
            "Press '/' to filter history...",
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

    // 2. Split main pane: history list (left) and preview/metadata (right)
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
                Span::styled(format!("{:<8}", rel_time), Style::default().fg(Color::DarkGray)),
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
                .title(" History "),
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
                .title(" Preview & Metadata ");
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
                        .format("%Y-%m-%d %H:%M:%S")
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
                            " | {w}x{h} px | {kb} KB | Uses: {} | Pinned: {}",
                            entry.use_count,
                            if entry.pinned { "Yes" } else { "No" },
                        ),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]),
                Line::from(Span::styled(
                    format!("Date: {formatted_date} | SHA: {short_hash}"),
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
                    Paragraph::new("Failed to decode image preview.")
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
                    format!(" | Expires in {}m{:02}s", rem / 60, rem % 60)
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
                        " | Uses: {} | Pinned: {} | SHA: {}{}",
                        entry.use_count,
                        if entry.pinned { "Yes" } else { "No" },
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
                        .title(" Preview & Metadata "),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(preview_widget, center[1]);
        }
    } else {
        app.image_preview_state = None;
        let preview_widget = Paragraph::new("No items found in history.")
            .style(Style::default().fg(Color::DarkGray))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Preview & Metadata "),
            );
        frame.render_widget(preview_widget, center[1]);
    }

    // 3. Bottom shortcuts / status bar
    let shortcuts =
        "Enter: Copy & Quit | v: View IMG | p: Pin | d: Delete | s: Secret | /: Search | Esc: Clear/Quit";
    let footer_line = if let Some(msg) = &app.status_message {
        Line::from(vec![
            Span::styled(format!("{msg}  |  "), Style::default().fg(Color::Green)),
            Span::styled(shortcuts, Style::default().fg(Color::Gray)),
        ])
    } else {
        Line::from(Span::styled(shortcuts, Style::default().fg(Color::Gray)))
    };

    let footer = Paragraph::new(footer_line)
        .block(Block::default().borders(Borders::ALL).title(" Shortcuts "));
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
            .insert_or_update("first normal item", now - 20)
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

        // Navigate to the SecretToken and reveal it
        app.handle_key_event(key(KeyCode::Down)).expect("down");
        assert_eq!(
            app.selected_entry().expect("second").kind,
            ContentKind::SecretToken
        );
        app.handle_key_event(key(KeyCode::Char('s'))).expect("reveal");
        assert!(app.reveal_secret);

        // Moving to another entry resets `reveal_secret` back to false for safety
        app.handle_key_event(key(KeyCode::Down)).expect("down 2");
        assert!(!app.reveal_secret);

        // Pin the third item and verify it moves to the top
        let third_id = app.selected_entry().expect("third").id;
        app.handle_key_event(key(KeyCode::Char('p'))).expect("pin");
        assert_eq!(app.entries[0].id, third_id);
        assert!(app.entries[0].pinned);

        // Delete the selected item and verify 2 items remain
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

        // First Esc clears the search query without quitting the TUI
        app.handle_key_event(key(KeyCode::Esc)).expect("esc clear");
        assert!(app.query.is_empty());
        assert!(!app.should_quit);
        assert_eq!(app.entries.len(), 3);

        // Second Esc quits the TUI
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
            .insert_or_update_image(png.clone(), 4, 4, now)
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

        app.handle_key_event(key(KeyCode::Char('v')))
            .expect("open external viewer");
        let saved = std::fs::read(PREVIEW_IMAGE_PATH).expect("read preview png");
        assert_eq!(saved, png);
    }

    #[test]
    fn resolve_protocol_type_enables_sixel_for_cosmic_and_kitty_for_modern_terminals() {
        assert_eq!(
            resolve_protocol_type(
                ProtocolType::Halfblocks,
                Some("cosmic-term"),
                Some("xterm-256color"),
                None,
            ),
            ProtocolType::Sixel
        );
        assert_eq!(
            resolve_protocol_type(
                ProtocolType::Halfblocks,
                None,
                Some("xterm-256color"),
                Some("1.0.0"),
            ),
            ProtocolType::Sixel
        );
        assert_eq!(
            resolve_protocol_type(
                ProtocolType::Halfblocks,
                None,
                Some("xterm-kitty"),
                None,
            ),
            ProtocolType::Kitty
        );
        assert_eq!(
            resolve_protocol_type(ProtocolType::Halfblocks, Some("ghostty"), None, None),
            ProtocolType::Kitty
        );
        assert_eq!(
            resolve_protocol_type(
                ProtocolType::Halfblocks,
                Some("WezTerm"),
                Some("xterm-256color"),
                None,
            ),
            ProtocolType::Kitty
        );
        assert_eq!(
            resolve_protocol_type(
                ProtocolType::Halfblocks,
                Some("gnome-terminal"),
                Some("xterm-256color"),
                None,
            ),
            ProtocolType::Halfblocks
        );
        assert_eq!(
            resolve_protocol_type(ProtocolType::Sixel, Some("kitty"), None, None),
            ProtocolType::Sixel
        );
    }
}
