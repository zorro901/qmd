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
//!   n              create a new note (enter "<collection>/<file>.md")
//!   e              edit the open note inline (tui-textarea)
//!   Esc            leave search · discard inline edit (asks if unsaved) · quit prompt
//!   Ctrl-S         save the inline edit (write file + reindex)
//!   Ctrl-R         reload the note list
//!   q              quit (asks if there are unsaved changes)
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

/// Pending confirmation for a destructive action, so we never silently lose
/// unsaved edits: quitting while dirty, or discarding an inline edit while dirty.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Confirm {
    Quit,
    CancelEdit,
}

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
    creating: bool,
    new_input: String,
    /// When set, the next Enter/yes confirms a destructive action and any other
    /// key (e.g. the same `q`/Esc) cancels it. Prevents silent data loss.
    confirm_pending: Option<Confirm>,
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
            creating: false,
            new_input: String::new(),
            confirm_pending: None,
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

    /// Begin creating a new note: pick the first collection, then prompt for a
    /// filename in `new_input`.
    fn start_create(&mut self) {
        match qmd::list_collections() {
            Ok(colls) if !colls.is_empty() => {
                // Prefill the input with the first collection name as a prefix.
                let (name, _) = &colls[0];
                self.new_input = format!("{name}/");
                self.creating = true;
                self.status = "new note — type a filename, Enter to create".into();
            }
            Ok(_) => self.status = "no collections; run 'qmd collection add .' first".into(),
            Err(e) => self.status = format!("error: {e}"),
        }
    }

    /// Finish the filename prompt: create the empty file inside its collection
    /// and open it in the inline editor.
    fn confirm_create(&mut self) {
        let raw = self.new_input.trim().to_string();
        self.creating = false;
        if raw.is_empty() {
            self.status = "cancelled".into();
            return;
        }
        // Split "<collection>/<path>" so we know which directory to write into.
        let (coll_name, rel) = match raw.split_once('/') {
            Some((c, p)) if !c.is_empty() && !p.is_empty() => (c.to_string(), p.to_string()),
            _ => {
                self.status = "use '<collection>/<file>.md' format".into();
                return;
            }
        };
        let file_name = if rel.ends_with(".md") {
            rel
        } else {
            format!("{rel}.md")
        };

        let colls = match qmd::list_collections() {
            Ok(c) => c,
            Err(e) => {
                self.status = format!("error: {e}");
                return;
            }
        };
        let dir = match colls.iter().find(|(n, _)| n == &coll_name) {
            Some((_, p)) => p.clone(),
            None => {
                self.status = format!("unknown collection '{coll_name}'");
                return;
            }
        };

        match qmd::create_note(&dir, &file_name, "") {
            Ok(abs) => {
                self.open_file = Some(format!("{coll_name}/{file_name}"));
                self.open_body = String::new();
                self.open_abs = Some(abs);
                self.textarea = TextArea::default();
                self.edit_mode = true;
                self.dirty = true;
                self.status = "new note — Ctrl-S save".into();
            }
            Err(e) => self.status = format!("create error: {e}"),
        }
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
                // Refresh the list so a freshly created/edited note shows up
                // (and the title reflects the new file) without a manual reload.
                self.reload_notes();
                // Keep the just-saved note selected in the list.
                if let Some(open) = &self.open_file {
                    if let Some(idx) = self.notes.iter().position(|n| &n.file == open) {
                        self.list_state.select(Some(idx));
                    }
                }
            }
            Err(e) => self.status = format!("save error: {e}"),
        }
    }

    /// Request to quit. If there are unsaved edits we arm a confirmation prompt
    /// instead of exiting, so work is never lost silently. Returns true when the
    /// caller (run_app) should actually break out of the loop.
    fn quit(&mut self) -> bool {
        if self.dirty {
            self.confirm_pending = Some(Confirm::Quit);
            self.status = "unsaved changes — Enter to quit without saving, q to cancel".into();
            false
        } else {
            true
        }
    }

    /// Handle a key event. Returns true when the app should quit. Centralized
    /// here so the logic is unit-testable without a terminal.
    fn handle_key(&mut self, key: event::KeyEvent) -> bool {
        // A confirmation prompt is active: Enter confirms, everything else
        // (including the same key that triggered it) cancels.
        if let Some(confirm) = self.confirm_pending {
            match key.code {
                KeyCode::Enter => {
                    self.confirm_pending = None;
                    match confirm {
                        Confirm::Quit => return true,
                        Confirm::CancelEdit => {
                            self.dirty = false;
                            self.edit_mode = false;
                            self.status = "edit discarded".into();
                        }
                    }
                }
                _ => {
                    self.confirm_pending = None;
                    self.status = "cancelled".into();
                }
            }
            return false;
        }

        // Inline-edit mode captures keys for tui-textarea.
        if self.edit_mode {
            match key.code {
                KeyCode::Esc => {
                    if self.dirty {
                        self.confirm_pending = Some(Confirm::CancelEdit);
                        self.status = "discard changes? Enter to discard, Esc to keep editing".into();
                    } else {
                        self.edit_mode = false;
                        self.status = "edit discarded".into();
                    }
                }
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.save_edit()
                }
                _ => {
                    self.textarea.input(Input::from(key));
                    self.dirty = true;
                }
            }
            return false;
        }

        // Search input mode captures everything.
        if self.searching {
            match key.code {
                KeyCode::Enter => {
                    self.searching = false;
                    self.run_search();
                }
                KeyCode::Esc => {
                    self.searching = false;
                    self.search_input.clear();
                }
                KeyCode::Char(c) => self.search_input.push(c),
                KeyCode::Backspace => {
                    self.search_input.pop();
                }
                _ => {}
            }
            return false;
        }

        // New-note filename prompt captures keys.
        if self.creating {
            match key.code {
                KeyCode::Enter => self.confirm_create(),
                KeyCode::Esc => {
                    self.creating = false;
                    self.new_input.clear();
                    self.status = "cancelled".into();
                }
                KeyCode::Char(c) => self.new_input.push(c),
                KeyCode::Backspace => {
                    self.new_input.pop();
                }
                _ => {}
            }
            return false;
        }

        match key.code {
            KeyCode::Char('q') => return self.quit(),
            KeyCode::Char('/') => self.searching = true,
            KeyCode::Char('e') => self.start_edit(),
            KeyCode::Char('n') => self.start_create(),
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => self.save_edit(),
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.searching = true
            }
            KeyCode::Enter => self.open_selected(),
            KeyCode::Down => {
                if !self.notes.is_empty() {
                    let i = self.list_state.selected().unwrap_or(0);
                    let next = (i + 1).min(self.notes.len() - 1);
                    self.list_state.select(Some(next));
                }
            }
            KeyCode::Up => {
                if !self.notes.is_empty() {
                    let i = self.list_state.selected().unwrap_or(0);
                    let prev = i.saturating_sub(1);
                    self.list_state.select(Some(prev));
                }
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.reload_notes()
            }
            _ => {}
        }
        false
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
                if app.handle_key(key) {
                    break;
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
    } else if app.creating {
        Line::from(vec![
            Span::styled("new › ", Style::default().fg(Color::Cyan)),
            Span::raw(&app.new_input),
            Span::styled("_", Style::default().fg(Color::Cyan)),
        ])
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
            let is_open = matches!(&app.open_file, Some(f) if f == &n.file);
            let mut spans = vec![
                Span::styled(title.to_string(), Style::default().add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(&n.file, Style::default().fg(Color::DarkGray)),
            ];
            if is_open {
                spans.push(Span::styled(
                    "  ◀ open",
                    Style::default().fg(Color::Cyan),
                ));
            }
            ListItem::new(Line::from(spans))
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
                 Keys: / search · ↑↓ move · Enter open · n new note · e edit inline · Ctrl-S save · q quit\n\
                 Unsaved edits: q asks, Esc in editor asks, Enter confirms discard/quit",
            ),
        };
        let para = Paragraph::new(text).block(block);
        f.render_widget(para, area);
    }
}

#[cfg(test)]
mod app_tests {
    use super::*;

    fn key(c: char) -> event::KeyEvent {
        event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty())
    }
    fn key_enter() -> event::KeyEvent {
        event::KeyEvent::new(KeyCode::Enter, KeyModifiers::empty())
    }
    fn key_esc() -> event::KeyEvent {
        event::KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())
    }

    // Headless integration test for the "new note" flow: start_create() ->
    // confirm_create() -> type into the textarea -> save_edit(). Verifies the
    // file is written with the typed content and reindexed. Skips without a
    // usable indexed collection (QMD_TUI_TEST_COLL_DIR + working qmd index).
    #[test]
    fn create_then_save_roundtrip() {
        let dir = match std::env::var("QMD_TUI_TEST_COLL_DIR") {
            Ok(d) => d,
            Err(_) => return,
        };
        let name = format!("qmd-tui-it-{}.md", std::process::id());
        let mut app = App::new();
        // Force the create prompt to target our test collection.
        app.start_create();
        if !app.creating {
            return; // no collections available in this index
        }
        // Prefill "<coll>/<name>" using the first collection prefix.
        app.new_input = if app.new_input.ends_with('/') {
            format!("{}{}", app.new_input, name)
        } else {
            format!("{}/{}", app.new_input, name)
        };
        app.confirm_create();
        assert!(app.open_abs.is_some(), "note should open after create");
        assert!(app.edit_mode, "should enter edit mode after create");

        // Type content and save.
        app.textarea = TextArea::from(["# Created in TUI\nhello integration\n"]);
        app.save_edit();
        assert!(!app.dirty, "save should clear dirty flag");

        let abs = app.open_abs.clone().unwrap();
        let content = std::fs::read_to_string(&abs).unwrap_or_default();
        assert!(
            content.contains("hello integration"),
            "saved file should contain typed text; got: {content:?}"
        );
        // Clean up: remove the file and reindex so the index stays consistent.
        let _ = std::fs::remove_file(&abs);
        let _ = qmd::save(&abs, "");
    }

    // Quit with unsaved edits must NOT exit (handle_key returns false); a second
    // 'q' should cancel the prompt, and only Enter after the prompt quits.
    #[test]
    fn quit_guards_unsaved_changes() {
        let mut app = App::new();
        app.dirty = true;
        // First 'q' arms the confirm prompt, does not quit.
        let quit = app.handle_key(key('q'));
        assert!(!quit, "quit must not happen while dirty");
        assert!(app.confirm_pending.is_some(), "confirm prompt must be armed");
        // A second 'q' cancels the prompt (not confirm).
        let quit2 = app.handle_key(key('q'));
        assert!(!quit2, "second q should cancel, not confirm");
        assert!(app.confirm_pending.is_none(), "prompt cancelled on q");
        assert!(app.dirty, "still dirty after cancel");
    }

    // Enter after the quit prompt actually quits (discarding unsaved edits).
    #[test]
    fn enter_confirms_quit() {
        let mut app = App::new();
        app.dirty = true;
        let _ = app.handle_key(key('q'));
        assert!(app.confirm_pending.is_some());
        let quit = app.handle_key(key_enter());
        assert!(quit, "Enter should confirm quit");
    }

    // Esc while editing dirty content arms a discard prompt; Enter discards.
    #[test]
    fn esc_while_editing_asks_before_discard() {
        let mut app = App::new();
        app.edit_mode = true;
        app.dirty = true;
        // Esc arms the CancelEdit prompt (does not leave edit mode yet).
        let quit = app.handle_key(key_esc());
        assert!(!quit);
        assert!(app.edit_mode, "edit mode stays until confirmed");
        assert!(app.confirm_pending.is_some());
        // Enter discards.
        let quit2 = app.handle_key(key_enter());
        assert!(!quit2);
        assert!(!app.edit_mode, "edit mode left after discard confirmed");
        assert!(!app.dirty, "dirty cleared after discard");
    }

    // Drive the real key path (handle_key) to type into the textarea, save via
    // Ctrl-S, and confirm: (a) the file gets the typed text, and (b) the note
    // list is refreshed so the new note appears. Needs a usable index.
    #[test]
    fn keypress_edit_then_save_persists() {
        let _ = std::env::var("QMD_TUI_TEST_COLL_DIR");
        let mut app = App::new();
        // Create a note, then exercise the key-driven editor on it.
        app.start_create();
        if !app.creating {
            return;
        }
        let name = format!("qmd-tui-key-{}.md", std::process::id());
        app.new_input = if app.new_input.ends_with('/') {
            format!("{}{}", app.new_input, name)
        } else {
            format!("{}/{}", app.new_input, name)
        };
        app.confirm_create();
        assert!(app.edit_mode, "should be editing after create");

        // Type char-by-char via handle_key (the real input path).
        for c in "hello via keys".chars() {
            app.handle_key(key(c));
        }
        assert!(app.dirty, "typing marks dirty");
        // Ctrl-S save.
        let ctrl_s = event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        );
        app.handle_key(ctrl_s);
        assert!(!app.dirty, "save clears dirty");

        let abs = app.open_abs.clone().unwrap();
        let content = std::fs::read_to_string(&abs).unwrap_or_default();
        assert!(
            content.contains("hello via keys"),
            "typed text should be saved; got: {content:?}"
        );
        // Save refreshes the list, so the freshly created note should be listed
        // and re-selected.
        let listed = app
            .notes
            .iter()
            .any(|n| n.file.ends_with(&name));
        assert!(listed, "new note should appear in the list after save");
        let sel = app.list_state.selected();
        let selected_is_open = sel
            .and_then(|i| app.notes.get(i))
            .map(|n| n.file.ends_with(&name))
            .unwrap_or(false);
        assert!(selected_is_open, "saved note should be re-selected in the list");
        // Clean up.
        let _ = std::fs::remove_file(&abs);
        let _ = qmd::save(&abs, "");
    }
}
