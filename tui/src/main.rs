//! qmd-tui: a SimpleNote-style terminal UI for qmd.
//!
//! Layout (left/right panes):
//!   ┌──────────────┬───────────────────────────┐
//!   │ search box   │                           │
//!   ├──────────────┤     note body / editor    │
//!   │ note list    │                           │
//!   └──────────────┴───────────────────────────┘
//!
//! Keys:
//!   /  or Ctrl-F   focus the search box
//!   ↑ ↓            move through the note list
//!   Enter          open the selected note in the right pane
//!   e              edit the open note in $EDITOR, then reindex on exit
//!   Ctrl-S         (same as `e`) save via external editor
//!   Esc            leave search / clear / quit from root
//!   q              quit
//!
//! All data goes through the `qmd` CLI (see qmd.rs); this TUI is a thin, fast
//! terminal front-end.

mod qmd;

use std::io::{self};
use std::process::Command;

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

    fn edit_external(&mut self) {
        let abs = match &self.open_abs {
            Some(p) => p.clone(),
            None => {
                self.status = "no note open".into();
                return;
            }
        };
        // Suspend the TUI, hand control to $EDITOR, then reindex on return.
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
        disable_raw_mode().ok();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let status = Command::new(&editor).arg(&abs).status();
        let _ = execute!(io::stdout(), EnterAlternateScreen);
        let _ = enable_raw_mode();

        match status {
            Ok(_) => {
                // Re-read the file (in case the editor changed it) and reindex.
                match qmd::save(&abs, &std::fs::read_to_string(&abs).unwrap_or_default()) {
                    Ok(()) => {
                        self.dirty = false;
                        if let Ok((body, _)) = qmd::get_body(
                            self.open_file.as_deref().unwrap_or(""),
                        ) {
                            self.open_body = body;
                        }
                        self.status = "saved & reindexed".into();
                    }
                    Err(e) => self.status = format!("reindex error: {e}"),
                }
            }
            Err(e) => self.status = format!("editor error: {e}"),
        }
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
                    KeyCode::Char('e') => app.edit_external(),
                    KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.edit_external()
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
    } else {
        Line::from(vec![
            Span::styled(
                "press / to search",
                Style::default().fg(Color::DarkGray),
            ),
        ])
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
                .title(format!("qmd  [{search_line}]  {}", app.status)),
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
    let (title, text) = match &app.open_file {
        Some(file) => {
            let marker = if app.dirty { " ●" } else { "" };
            (
                format!(" {file}{marker} "),
                Text::from(app.open_body.clone()),
            )
        }
        None => (
            " no note open ".into(),
            Text::from("Select a note on the left, then press Enter to open it.\n\nKeys: / search · ↑↓ move · Enter open · e edit ($EDITOR) · Ctrl-S save · q quit"),
        ),
    };

    let para = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll((0, 0));
    f.render_widget(para, area);
}
