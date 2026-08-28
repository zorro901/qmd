//! qmd-tui: a SimpleNote-style terminal UI for qmd.
//!
//! Layout (left/right panes):
//!   ┌──────────────┬───────────────────────────┐
//!   │ search box   │                           │
//!   ├──────────────┤   note body / editor      │
//!   │ note list    │   (inline edit w/ tui-    │
//!   └──────────────┤    textarea)              │
//!                  └───────────────────────────┘
//!
//! Keys:
//!   /  or Ctrl-F   focus the search box
//!   ↑ ↓            move through the note list
//!   Enter          open the selected note in the right pane
//!   e              edit the open note inline (tui-textarea)
//!   Esc            leave search · cancel inline edit (discard) · quit from root
//!   Ctrl-S         save the inline edit (write file + reindex)
//!   Ctrl-R         reload the note list
//!   q              quit
//!
//! All data goes through the `qmd` CLI (see qmd.rs); this TUI is a thin, fast
//! terminal front-end.

mod qmd;

use std::io;

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame, Terminal,
};
use tui_textarea::{Input, TextArea};

struct App {
    notes: Vec<qmd::Note>,
    list_state: ListState,
    search_input: String,
    searching: bool,
    status: String,
    open_file: Option<String>,
    open_body: String,
    open_abs: Option<std::path::PathBuf>,
    dirty: bool,
    edit_mode: bool,
    textarea: TextArea<'static>,
}

impl App {
    fn new() -> App {
        let mut app = App {
            notes: Vec::new(),
            list_state: ListState::default(),
            search_input: String::new(),
            searching: false,
            status: "loading…".into(),
            open_file: None,
            open_body: String::new(),
            open_abs: None,
            dirty: false,
            edit_mode: false,
            textarea: TextArea::default(),
        };
        app.reload_notes();
        app
    }

    fn reload_notes(&mut self) {
        match qmd::list_notes() {
            Ok(notes) => {
                self.notes = notes;
                if self.notes.is_empty() {
                    self.status = "no notes — run 'qmd collection add .' then 'qmd update'".into();
                } else {
                    self.status = format!("{} notes", self.notes.len());
                }
                if self.list_state.selected().is_none() && !self.notes.is_empty() {
                    self.list_state.select(Some(0));
                }
            }
            Err(e) => self.status = format!("error: {e}"),
        }
    }

    fn run_search(&mut self) {
        let q = self.search_input.trim();
        if q.is_empty() {
            self.reload_notes();
            return;
        }
        match qmd::search(q) {
            Ok(notes) => {
                self.notes = notes;
                self.status = format!("{} results for '{}'", self.notes.len(), q);
                if self.notes.is_empty() {
                    self.list_state.select(None);
                } else {
                    self.list_state.select(Some(0));
                }
            }
            Err(e) => self.status = format!("search error: {e}"),
        }
    }

    fn open_selected(&mut self) {
        let idx = match self.list_state.selected() {
            Some(i) => i,
            None => return,
        };
        let note = match self.notes.get(idx) {
            Some(n) => n.clone(),
            None => return,
        };
        match qmd::get_body(&note.file) {
            Ok((body, abs)) => {
                self.open_file = Some(note.file.clone());
                self.open_body = body;
                self.open_abs = abs;
                self.dirty = false;
                self.status = format!("opened {}", note.file);
            }
            Err(e) => self.status = format!("open error: {e}"),
        }
    }

    /// Load the open note into the inline editor.
    fn start_edit(&mut self) {
        if self.open_file.is_none() {
            self.status = "open a note first (Enter), then press e to edit".into();
            return;
        }
        self.textarea = TextArea::from(self.open_body.split('\n'));
        self.edit_mode = true;
        self.status = "editing — Ctrl-S save · Esc cancel".into();
    }

    /// Persist the inline edit: write the file, then reindex just that file.
    fn save_edit(&mut self) {
        let abs = match &self.open_abs {
            Some(p) => p.clone(),
            None => {
                self.status = "no note open".into();
                return;
            }
        };
        let content: String = self.textarea.lines().join("\n");
        match qmd::save(&abs, &content) {
            Ok(()) => {
                self.open_body = content;
                self.dirty = false;
                self.edit_mode = false;
                self.status = "saved & reindexed".into();
            }
            Err(e) => self.status = format!("save error: {e}"),
        }
    }

    /// Discard the inline edit and return to view mode.
    fn cancel_edit(&mut self) {
        self.edit_mode = false;
        self.status = "edit discarded".into();
    }
}

fn main() -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let app = App::new();
    let res = run_app(&mut terminal, app);

    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    if let Err(e) = res {
        eprintln!("qmd-tui error: {e}");
    }
    Ok(())
}

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    mut app: App,
) -> io::Result<()> {
    loop {
        terminal.draw(|f| ui(f, &mut app))?;

        if event::poll(std::time::Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                // Inline-edit mode captures keys for tui-textarea.
                if app.edit_mode {
                    match key.code {
                        KeyCode::Esc => app.cancel_edit(),
                        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            app.save_edit()
                        }
                        _ => {
                            app.textarea.input(Input::from(key));
                            app.dirty = true;
                        }
                    }
                    continue;
                }

                // Search input mode captures everything.
                if app.searching {
                    match key.code {
                        KeyCode::Enter => {
                            app.searching = false;
                            app.run_search();
                        }
                        KeyCode::Esc => {
                            app.searching = false;
                            app.search_input.clear();
                        }
                        KeyCode::Char(c) => app.search_input.push(c),
                        KeyCode::Backspace => {
                            app.search_input.pop();
                        }
                        _ => {}
                    }
                    continue;
                }

                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Char('/') => app.searching = true,
                    KeyCode::Char('e') => app.start_edit(),
                    KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.save_edit()
                    }
                    KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.searching = true
                    }
                    KeyCode::Enter => app.open_selected(),
                    KeyCode::Down => {
                        if !app.notes.is_empty() {
                            let i = app.list_state.selected().unwrap_or(0);
                            let next = (i + 1).min(app.notes.len() - 1);
                            app.list_state.select(Some(next));
                        }
                    }
                    KeyCode::Up => {
                        if !app.notes.is_empty() {
                            let i = app.list_state.selected().unwrap_or(0);
                            let prev = i.saturating_sub(1);
                            app.list_state.select(Some(prev));
                        }
                    }
                    KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.reload_notes()
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn ui(f: &mut Frame<'_>, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(f.area());

    render_list(f, app, chunks[0]);
    render_body(f, app, chunks[1]);
}

fn render_list(f: &mut Frame<'_>, app: &mut App, area: Rect) {
    let search_line = if app.searching {
        Line::from(vec![
            Span::styled("› ", Style::default().fg(Color::Yellow)),
            Span::raw(&app.search_input),
            Span::styled("_", Style::default().fg(Color::Yellow)),
        ])
    } else if app.edit_mode {
        Line::from(vec![Span::styled(
            "editing — Ctrl-S save · Esc cancel",
            Style::default().fg(Color::Green),
        )])
    } else {
        Line::from(vec![Span::styled(
            "press / to search",
            Style::default().fg(Color::DarkGray),
        )])
    };

    let items: Vec<ListItem> = app
        .notes
        .iter()
        .map(|n| {
            let title = if n.title.is_empty() { &n.file } else { &n.title };
            ListItem::new(Line::from(vec![
                Span::styled(title.to_string(), Style::default().add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(&n.file, Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("qmd  [{}]  {}", search_line, app.status)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(40, 44, 52))
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_body(f: &mut Frame<'_>, app: &mut App, area: Rect) {
    let title = match &app.open_file {
        Some(file) => {
            let marker = if app.dirty { " ●" } else { "" };
            format!(" {file}{marker} ")
        }
        None => " no note open ".into(),
    };
    let block = Block::default().borders(Borders::ALL).title(title);

    if app.edit_mode {
        // Inline editor: render the textarea with the same border/title.
        let mut ta = app.textarea.clone();
        ta.set_block(block);
        f.render_widget(ta.widget(), area);
    } else {
        let text = match &app.open_file {
            Some(_) => Text::from(app.open_body.clone()),
            None => Text::from(
                "Select a note on the left, then press Enter to open it.\n\n\
                 Keys: / search · ↑↓ move · Enter open · e edit inline · Ctrl-S save · q quit",
            ),
        };
        let para = Paragraph::new(text).block(block);
        f.render_widget(para, area);
    }
}
