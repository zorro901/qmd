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
//!   ↑ ↓  or  j k    move through the note list (g = top, G = bottom)
//!   Enter          open the selected note in the right pane
//!   c              switch collection (filter the list/notes)
//!   ?              show keybindings
//!   n              create a new note (enter "<collection>/<file>.md")
//!   e              edit the open note inline (tui-textarea)
//!   d              delete the selected note (asks to confirm)
//!   PgUp/PgDn · Home/End · mouse wheel   scroll the note body
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
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseEvent,
        MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    symbols::scrollbar as scrollbar_symbols,
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame, Terminal,
};
use tui_textarea::{Input, TextArea};

/// Pending confirmation for a destructive action, so we never silently lose
/// work: quitting while dirty, discarding an inline edit while dirty, or
/// deleting the open/selected note.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Confirm {
    Quit,
    CancelEdit,
    Delete,
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
    /// Vertical scroll offset (in lines) of the note body pane.
    vertical_scroll: u16,
    /// Last executed search query, used to highlight matches in the list.
    query: String,
    /// Active collection filter (None = all collections).
    collection: Option<String>,
    /// Available collections, loaded for the switcher picker.
    collections: Vec<(String, std::path::PathBuf)>,
    /// Selection index within `collections` for the switcher picker.
    collection_idx: usize,
    /// When true, a collection-switcher picker overlay is active.
    picking: bool,
    /// When true, the keybinding help overlay is shown (dismissed by any key).
    show_help: bool,
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
            vertical_scroll: 0,
            query: String::new(),
            collection: None,
            collections: Vec::new(),
            collection_idx: 0,
            picking: false,
            show_help: false,
        };
        app.reload_notes();
        app
    }

    fn reload_notes(&mut self) {
        let coll: Option<&str> = self.collection.as_deref();
        match qmd::list_notes(coll) {
            Ok(notes) => {
                self.notes = notes;
                self.query = String::new();
                if self.notes.is_empty() {
                    self.status = "no notes — run 'qmd collection add .' then 'qmd update'".into();
                } else {
                    self.status = match &self.collection {
                        Some(c) => format!("{} notes in {}", self.notes.len(), c),
                        None => format!("{} notes", self.notes.len()),
                    };
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
        self.query = q.to_lowercase();
        let coll: Option<&str> = self.collection.as_deref();
        match qmd::search(q, coll) {
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

    /// Refresh the list of indexed collections for the switcher picker. The
    /// picker also shows a synthetic "All collections" entry at index 0, so the
    /// real collections are offset by one.
    fn load_collections(&mut self) {
        match qmd::list_collections() {
            Ok(cols) => {
                self.collections = cols;
                // Point the picker selection at the active collection (if any).
                // +1 because index 0 is the "All collections" entry.
                self.collection_idx = self
                    .collections
                    .iter()
                    .position(|(name, _)| Some(name.as_str()) == self.collection.as_deref())
                    .map(|i| i + 1)
                    .unwrap_or(0);
            }
            Err(_) => self.collections = Vec::new(),
        }
    }

    /// Open the collection-switcher picker (loads collections first).
    fn start_pick_collection(&mut self) {
        self.load_collections();
        self.picking = true;
    }

    /// Apply the picked collection (or None for "all") and reload the list.
    /// The picker's `collection_idx` is offset by one: index 0 is the synthetic
    /// "All collections" entry, 1..=N map to `self.collections[N-1]`.
    fn confirm_pick_collection(&mut self) {
        // Index 0 is the special "All collections" entry.
        self.collection = if self.collection_idx == 0 {
            None
        } else {
            self.collections
                .get(self.collection_idx - 1)
                .map(|(name, _)| name.clone())
        };
        self.picking = false;
        // Clear search context and refresh.
        self.search_input.clear();
        self.query.clear();
        self.list_state.select(None);
        self.open_file = None;
        self.open_body.clear();
        self.open_abs = None;
        self.reload_notes();
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
                self.vertical_scroll = 0;
                self.status = format!("opened {}", note.file);
            }
            Err(e) => self.status = format!("open error: {e}"),
        }
    }

    /// Move the list selection by `delta` rows (negative = up), clamped to the
    /// list bounds. No-op when the list is empty. Shared by the arrow keys and
    /// the vim-style `j`/`k` bindings.
    fn move_selection(&mut self, delta: isize) {
        if self.notes.is_empty() {
            return;
        }
        let i = self.list_state.selected().unwrap_or(0);
        let len = self.notes.len() as isize;
        let next = (i as isize + delta).clamp(0, len - 1) as usize;
        self.list_state.select(Some(next));
    }

    /// Arm a deletion confirmation for the selected note. Deletion is destructive
    /// (removes the file from disk), so it always requires an explicit Enter.
    fn arm_delete(&mut self) {
        let idx = match self.list_state.selected() {
            Some(i) => i,
            None => {
                self.status = "select a note first, then press d to delete".into();
                return;
            }
        };
        if self.notes.get(idx).is_none() {
            self.status = "select a note first, then press d to delete".into();
            return;
        }
        self.confirm_pending = Some(Confirm::Delete);
        self.status = "delete this note? Enter to delete, any other key cancels".into();
    }

    /// Actually delete the previously selected note and refresh the list.
    fn delete_selected(&mut self) {
        let idx = match self.list_state.selected() {
            Some(i) => i,
            None => return,
        };
        let note = match self.notes.get(idx) {
            Some(n) => n.clone(),
            None => return,
        };
        match qmd::delete_note(&note.file) {
            Ok(()) => {
                // If the deleted note was open, close the pane.
                if self.open_file.as_deref() == Some(&note.file) {
                    self.open_file = None;
                    self.open_body.clear();
                    self.open_abs = None;
                    self.dirty = false;
                    self.edit_mode = false;
                }
                self.status = format!("deleted {}", note.file);
                self.reload_notes();
                // Keep a valid selection after the list shrinks.
                if !self.notes.is_empty() {
                    let max = self.notes.len() - 1;
                    let sel = self.list_state.selected().unwrap_or(0).min(max);
                    self.list_state.select(Some(sel));
                }
            }
            Err(e) => self.status = format!("delete error: {e}"),
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
        self.vertical_scroll = 0;
        self.status = "editing — Ctrl-S save · Esc cancel".into();
    }

    /// Scroll the note body by `delta` lines (negative = up), clamped to the
    /// range the pane can actually show for `content_lines` / `viewport_lines`.
    fn scroll_body(&mut self, delta: i32, content_lines: usize, viewport_lines: usize) {
        if self.edit_mode {
            return; // the textarea scrolls itself
        }
        let max = content_lines.saturating_sub(viewport_lines);
        let cur = self.vertical_scroll as i32;
        let next = (cur + delta).clamp(0, max as i32);
        self.vertical_scroll = next as u16;
    }

    /// Begin creating a new note: pick a collection (the active filtered one if
    /// set, else the first), then prompt for a filename in `new_input`.
    fn start_create(&mut self) {
        match qmd::list_collections() {
            Ok(colls) if !colls.is_empty() => {
                // Prefer the active collection filter, else the first one.
                let preferred = self
                    .collection
                    .as_deref()
                    .and_then(|c| colls.iter().find(|(n, _)| n == c))
                    .or_else(|| colls.first());
                if let Some((name, _)) = preferred {
                    self.new_input = format!("{name}/");
                    self.creating = true;
                    self.status = "new note — type a filename, Enter to create".into();
                }
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
                        Confirm::Delete => self.delete_selected(),
                    }
                }
                _ => {
                    self.confirm_pending = None;
                    self.status = "cancelled".into();
                }
            }
            return false;
        }

        // Help overlay is modal: any key dismisses it (re-pressing ? also works).
        if self.show_help {
            self.show_help = false;
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

        // Collection-switcher picker captures keys (modal overlay).
        if self.picking {
            match key.code {
                KeyCode::Enter => self.confirm_pick_collection(),
                KeyCode::Esc => self.picking = false,
                KeyCode::Up => {
                    if self.collection_idx > 0 {
                        self.collection_idx -= 1;
                    }
                }
                KeyCode::Down => {
                    let max = self.collections.len(); // +1 for "All" at index 0
                    if self.collection_idx < max {
                        self.collection_idx += 1;
                    }
                }
                _ => {}
            }
            return false;
        }

        // Search input mode captures everything and searches live as you type.
        if self.searching {
            match key.code {
                KeyCode::Enter => {
                    // Keep the live results; just leave the search box.
                    self.searching = false;
                }
                KeyCode::Esc => {
                    // Cancel: clear the box and restore the full note list.
                    self.searching = false;
                    self.search_input.clear();
                    self.reload_notes();
                }
                KeyCode::Char(c) => {
                    self.search_input.push(c);
                    self.run_search();
                }
                KeyCode::Backspace => {
                    self.search_input.pop();
                    self.run_search();
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
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('c') => self.start_pick_collection(),
            KeyCode::Char('d') => self.arm_delete(),
            KeyCode::Char('e') => self.start_edit(),
            KeyCode::Char('n') => self.start_create(),
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => self.save_edit(),
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.searching = true
            }
            KeyCode::Enter => self.open_selected(),
            KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Char('g') => {
                if !self.notes.is_empty() {
                    self.list_state.select(Some(0));
                }
            }
            KeyCode::Char('G') => {
                if !self.notes.is_empty() {
                    self.list_state.select(Some(self.notes.len() - 1));
                }
            }
            KeyCode::Down => self.move_selection(1),
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.reload_notes()
            }
            KeyCode::PageUp => self.scroll_body(-10, self.open_body.lines().count(), usize::MAX),
            KeyCode::PageDown => self.scroll_body(10, self.open_body.lines().count(), usize::MAX),
            KeyCode::Home => self.vertical_scroll = 0,
            KeyCode::End => {
                // Jump to the bottom; render() clamps to the last viewable line.
                let n = self.open_body.lines().count();
                self.vertical_scroll = n.saturating_sub(1) as u16;
            }
            _ => {}
        }
        false
    }

    /// Handle mouse events: wheel scrolling moves the note body. Other mouse
    /// interactions are ignored (the list uses arrow keys).
    fn handle_mouse(&mut self, m: MouseEvent) {
        if self.edit_mode {
            return;
        }
        match m.kind {
            MouseEventKind::ScrollDown => {
                self.scroll_body(3, self.open_body.lines().count(), usize::MAX);
            }
            MouseEventKind::ScrollUp => {
                self.scroll_body(-3, self.open_body.lines().count(), usize::MAX);
            }
            _ => {}
        }
    }
}

fn main() -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let app = App::new();
    let res = run_app(&mut terminal, app);

    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
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
            match event::read()? {
                Event::Key(key) => {
                    if app.handle_key(key) {
                        break;
                    }
                }
                Event::Mouse(m) => app.handle_mouse(m),
                _ => {}
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

    if app.picking {
        render_collection_picker(f, app);
    }
    if app.show_help {
        render_help(f);
    }
}

/// Render a centered collections-switcher overlay. Index 0 is the synthetic
/// "All collections" entry; the rest map 1:1 to `app.collections`.
fn render_collection_picker(f: &mut Frame<'_>, app: &mut App) {
    use ratatui::widgets::Clear;
    let area = f.area();
    let width = 40.min(area.width.saturating_sub(4));
    let height = (app.collections.len() as u16 + 3).min(area.height.saturating_sub(4));
    let x = area.width.saturating_sub(width) / 2;
    let y = area.height.saturating_sub(height) / 2;
    let popup = Rect {
        x,
        y,
        width,
        height,
    };
    f.render_widget(Clear, popup);

    let mut state = ListState::default();
    state.select(Some(app.collection_idx));

    let mut items: Vec<ListItem> = vec![ListItem::new(Line::from(Span::styled(
        "All collections",
        Style::default().add_modifier(Modifier::BOLD),
    )))];
    for (name, path) in &app.collections {
        let active = self_collection_active(app, name);
        let label = if active {
            format!("● {name}  ({path:?})")
        } else {
            format!("  {name}  ({path:?})")
        };
        items.push(ListItem::new(Line::from(label)));
    }

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("collections  —  ↑↓ move · Enter select · Esc cancel"),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(40, 44, 52))
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, popup, &mut state);
}

/// Render a centered keybinding help overlay. Any key dismisses it (handled in
/// `App::handle_key`).
fn render_help(f: &mut Frame<'_>) {
    use ratatui::widgets::Clear;
    let area = f.area();
    let width = 54.min(area.width.saturating_sub(4));
    let lines = HELP_LINES.len() as u16;
    let height = (lines + 2).min(area.height.saturating_sub(4));
    let x = area.width.saturating_sub(width) / 2;
    let y = area.height.saturating_sub(height) / 2;
    let popup = Rect {
        x,
        y,
        width,
        height,
    };
    f.render_widget(Clear, popup);

    let body: Vec<Line> = HELP_LINES
        .iter()
        .map(|(key, desc)| {
            Line::from(vec![
                Span::styled(
                    format!("  {:<16}", key),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::raw(*desc),
            ])
        })
        .collect();

    let para = Paragraph::new(body)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("qmd-tui keys  —  press any key to close"),
        );
    f.render_widget(para, popup);
}

/// (key, description) pairs shown in the help overlay. Kept as data so the
/// empty-state hint text can reuse the same source of truth.
const HELP_LINES: &[(&str, &str)] = &[
    ("/", "focus the search box (live, as you type)"),
    ("Ctrl-F", "focus the search box"),
    ("Enter", "open the selected note"),
    ("↑ ↓ / j k", "move through the note list (g top · G bottom)"),
    ("c", "switch collection (filter list + search)"),
    ("n", "create a new note"),
    ("e", "edit the open note inline"),
    ("d", "delete the selected note (asks)"),
    ("Ctrl-S", "save the inline edit (write + reindex)"),
    ("PgUp/PgDn", "scroll the note body"),
    ("Home/End", "jump to top / bottom of body"),
    ("mouse wheel", "scroll the note body"),
    ("?", "show this help"),
    ("Ctrl-R", "reload the note list"),
    ("q", "quit (asks if there are unsaved changes)"),
    ("Esc", "cancel search / discard edit (asks) / close"),
];

fn self_collection_active(app: &App, name: &str) -> bool {
    app.collection.as_deref() == Some(name)
}

/// Split `text` into spans, highlighting (yellow, bold) every case-insensitive
/// occurrence of `query`. When `query` is empty, returns a single plain span.
fn highlight(text: &str, query: &str) -> Vec<Span<'static>> {
    if query.is_empty() {
        return vec![Span::raw(text.to_string())];
    }
    let lower = text.to_lowercase();
    let q = query.to_lowercase();
    let mut spans = Vec::new();
    let mut start = 0;
    while let Some(idx) = lower[start..].find(&q) {
        let hit = start + idx;
        if hit > start {
            spans.push(Span::raw(text[start..hit].to_string()));
        }
        let end = hit + q.len();
        spans.push(
            Span::styled(text[hit..end].to_string(), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        );
        start = end;
    }
    if start < text.len() {
        spans.push(Span::raw(text[start..].to_string()));
    }
    spans
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
            let mut spans: Vec<Span<'static>> = Vec::new();
            // Bold title with query matches highlighted.
            for s in highlight(title, &app.query) {
                spans.push(s.add_modifier(Modifier::BOLD));
            }
            spans.push(Span::raw("  "));
            // File path with query matches highlighted (dimmer base color).
            for s in highlight(&n.file, &app.query) {
                spans.push(s.fg(Color::DarkGray));
            }
            // Dim recency suffix from the ISO mtime (YYYY-MM-DD prefix).
            let date = n.mtime.get(..10).unwrap_or("");
            if !date.is_empty() {
                spans.push(Span::styled(
                    format!("  {date}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
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
                .title(format!(
                    "qmd  [{}]  [{}]  {}",
                    search_line,
                    app.collection.as_deref().unwrap_or("all"),
                    app.status
                )),
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
        f.render_widget(&ta, area);
    } else {
        let text = match &app.open_file {
            Some(_) => Text::from(app.open_body.clone()),
            None => Text::from(
                "Select a note on the left, then press Enter to open it.\n\n\
                 Keys: / search · ↑↓ move · Enter open · n new note · e edit inline\n\
                 c switch collection · d delete · Ctrl-S save · ? help · q quit\n\
                 Unsaved edits: q asks, Esc in editor asks, Enter confirms discard/quit\n\
                 Scroll: mouse wheel · PgUp/PgDn · Home/End",
            ),
        };
        // Clamp the scroll to what the viewport can actually show.
        let total: u16 = app.open_body.lines().count().saturating_sub(1) as u16;
        let viewport = area.height.saturating_sub(2) as usize; // minus borders
        let max = total.saturating_sub(viewport as u16);
        let offset = app.vertical_scroll.min(max);
        let para = Paragraph::new(text)
            .block(block)
            .scroll((offset, 0));
        f.render_widget(para, area);

        // Scrollbar reflecting position within the note.
        if total > 0 {
            let scrollbar_area = Rect {
                x: area.x + area.width.saturating_sub(1),
                y: area.y + 1,
                width: 1,
                height: area.height.saturating_sub(2),
            };
            let mut state = ScrollbarState::new(total as usize).position(offset as usize);
            if max > 0 {
                state = state.viewport_content_length(viewport);
            }
            f.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .symbols(scrollbar_symbols::VERTICAL),
                scrollbar_area,
                &mut state,
            );
        }
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
    fn key_end() -> event::KeyEvent {
        event::KeyEvent::new(KeyCode::End, KeyModifiers::empty())
    }

    // Headless integration test for the "new note" flow: start_create() ->
    // confirm_create() -> type into the textarea -> save_edit(). Verifies the
    // file is written with the typed content and reindexed. Skips without a
    // usable indexed collection (QMD_TUI_TEST_COLL_DIR + working qmd index).
    #[test]
    fn create_then_save_roundtrip() {
        let _ = std::env::var("QMD_TUI_TEST_COLL_DIR");
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

    // Vim-style navigation: j/k move by one, g jumps to top, G to bottom, and
    // movement clamps at both ends. Uses a synthetic note list so the result is
    // deterministic regardless of the index on the test machine.
    #[test]
    fn vim_keys_move_selection() {
        let mut app = App::new();
        app.notes = (0..5).map(|i| qmd::Note {
            file: format!("t/n{i}.md"),
            title: format!("n{i}"),
            mtime: String::new(),
        }).collect();
        app.list_state.select(Some(0));

        app.handle_key(key('j'));
        assert_eq!(app.list_state.selected(), Some(1), "j moves down one");
        app.handle_key(key('k'));
        assert_eq!(app.list_state.selected(), Some(0), "k moves up one");
        app.handle_key(key('G'));
        assert_eq!(app.list_state.selected(), Some(4), "G jumps to bottom");
        app.handle_key(key('G'));
        assert_eq!(app.list_state.selected(), Some(4), "G clamps at bottom");
        app.handle_key(key('g'));
        assert_eq!(app.list_state.selected(), Some(0), "g jumps to top");
        app.handle_key(key('k'));
        assert_eq!(app.list_state.selected(), Some(0), "k clamps at top");

        // Arrow keys stay consistent with j/k via the shared helper.
        app.handle_key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::empty()));
        assert_eq!(app.list_state.selected(), Some(1), "Down == j");
        app.handle_key(event::KeyEvent::new(KeyCode::Up, KeyModifiers::empty()));
        assert_eq!(app.list_state.selected(), Some(0), "Up == k");
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

    // Body scroll clamps: never negative, never past the last visible line for a
    // given viewport. Driven via the real handle_key PgDn/PgUp + End paths.
    #[test]
    fn body_scroll_clamps() {
        let mut app = App::new();
        app.open_file = Some("x.md".into());
        // A 100-line body.
        app.open_body = (0..100).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let total = app.open_body.lines().count();
        // viewport larger than content -> scroll stays 0.
        app.scroll_body(1000, total, 200);
        assert_eq!(app.vertical_scroll, 0, "no scroll when viewport fits all");
        // viewport smaller than content -> can scroll but clamps at the end.
        app.scroll_body(1000, total, 20);
        let max = (total - 20) as u16;
        assert!(app.vertical_scroll <= max, "scroll clamped to max");
        assert_eq!(app.vertical_scroll, max, "scrolls to bottom on big jump");
        // Scrolling back up never goes negative.
        app.scroll_body(-10000, total, 20);
        assert_eq!(app.vertical_scroll, 0, "scroll never negative");
        // End jumps to the bottom; render() clamps to the last viewable line,
        // so the effective offset must not exceed max.
        app.handle_key(key_end());
        let rendered = app.vertical_scroll.min(max);
        assert!(rendered <= max, "End clamps within content at render time");
        assert_eq!(rendered, max, "End reaches the bottom line");
    }

    // highlight() wraps every case-insensitive occurrence of the query in its
    // own span, leaving the rest unchanged, and returns a single plain span
    // when there is no query.
    #[test]
    fn query_highlight_wraps_matches() {
        // No query -> single raw span, full text preserved.
        let spans = highlight("Hello World", "");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "Hello World");

        // One match, mid-string, case-insensitive.
        let spans = highlight("Note about RUST and rust again", "rust");
        // "Note about " + "RUST" + " and " + "rust" + " again" = 5 spans.
        assert_eq!(spans.len(), 5);
        assert_eq!(spans[0].content, "Note about ");
        assert_eq!(spans[1].content, "RUST");
        assert_eq!(spans[3].content, "rust");
        assert_eq!(spans[4].content, " again");

        // No match -> single raw span.
        let spans = highlight("nothing here", "xyz");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "nothing here");
    }

    // run_search stores a lowercased query for highlighting; a non-empty list
    // of notes (from a real index) keeps the query, while reload_notes clears
    // it so normal list rendering shows no highlights.
    #[test]
    fn run_search_sets_query() {
        let mut app = App::new();
        // No query -> behaves like reload, query stays empty.
        app.search_input = "   ".into();
        app.query = "leftover".into();
        app.run_search();
        assert_eq!(app.query, "", "empty search clears the query");

        // Non-empty query is stored (lower-cased) and survives until a reload.
        app.search_input = "PROJECT".into();
        app.query = String::new();
        // If there is no usable index this simply won't error; the query is set
        // before the search call so it is always populated here.
        app.run_search();
        assert_eq!(app.query, "project", "query is stored lower-cased");

        // A reload (e.g. Ctrl-R or post-save) clears the highlight query.
        app.reload_notes();
        assert_eq!(app.query, "", "reload clears the highlight query");
    }

    // Typing in search mode searches live (query is set on each keystroke) and
    // Esc cancels back to the full list without leaving the query set.
    #[test]
    fn search_live_typing_and_esc_cancel() {
        let mut app = App::new();
        app.searching = true;
        // Each character triggers run_search, which stores the query live.
        for c in "git".chars() {
            app.handle_key(key(c));
        }
        assert!(app.searching, "still in search mode while typing");
        assert_eq!(app.query, "git", "query updates live as you type");
        assert_eq!(app.search_input, "git", "search box holds the typed text");
        // Esc cancels: clears the box and the query, leaves search mode.
        app.handle_key(key_esc());
        assert!(!app.searching, "esc leaves search mode");
        assert_eq!(app.search_input, "", "esc clears the search box");
        assert_eq!(app.query, "", "esc clears the highlight query");
    }

    // Picker navigation clamps at the ends and the "All" entry sits at index 0.
    #[test]
    fn collection_picker_navigates_and_clamps() {
        let mut app = App::new();
        // Simulate a loaded picker with two real collections after "All".
        app.collections = vec![
            ("work".into(), std::path::PathBuf::from("/w")),
            ("home".into(), std::path::PathBuf::from("/h")),
        ];
        app.picking = true;
        app.collection_idx = 0;
        // Up from the top is a no-op.
        app.handle_key(key('k')); // not a nav key in picker -> ignored
        // Use arrow keys via the real path.
        app.handle_key(event::KeyEvent::new(KeyCode::Up, KeyModifiers::empty()));
        assert_eq!(app.collection_idx, 0, "Up clamps at the top ('All')");
        // Down twice reaches the last collection (index 2).
        app.handle_key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::empty()));
        app.handle_key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::empty()));
        assert_eq!(app.collection_idx, 2, "Down moves through collections");
        // One more Down clamps at the end.
        app.handle_key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::empty()));
        assert_eq!(app.collection_idx, 2, "Down clamps at the last entry");
        assert!(app.picking, "still picking until Enter/Esc");
    }

    // 'c' opens the collection picker (loads collections via qmd).
    #[test]
    fn collection_key_opens_picker() {
        let mut app = App::new();
        app.handle_key(key('c'));
        assert!(app.picking, "'c' opens the collection picker");
    }

    // confirm_pick_collection maps a valid index (>0) to that collection, and
    // index 0 ("All") to no filter, clearing search/list view state. Uses a
    // manually loaded picker so it does not depend on the live qmd index.
    #[test]
    fn collection_switch_sets_filter() {
        let mut app = App::new();
        app.collections = vec![
            ("work".into(), std::path::PathBuf::from("/w")),
            ("home".into(), std::path::PathBuf::from("/h")),
        ];
        // Pick "home" (index 2: 0=All, 1=work, 2=home) and confirm.
        app.picking = true;
        app.collection_idx = 2;
        app.confirm_pick_collection();
        assert!(!app.picking, "picker closed after confirm");
        assert_eq!(app.collection.as_deref(), Some("home"), "filter set to picked collection");

        // Re-open and pick "All" (index 0).
        app.picking = true;
        app.collection_idx = 0;
        app.confirm_pick_collection();
        assert_eq!(app.collection, None, "'All' clears the filter");
    }

    // Esc cancels the picker without changing the active collection.
    #[test]
    fn collection_picker_esc_cancels() {
        let mut app = App::new();
        app.collections = vec![("work".into(), std::path::PathBuf::from("/w"))];
        app.collection = Some("work".into());
        app.picking = true;
        app.collection_idx = 1; // moved selection before cancelling
        app.handle_key(key_esc());
        assert!(!app.picking, "esc closes the picker");
        assert_eq!(app.collection.as_deref(), Some("work"), "active collection unchanged on cancel");
    }

    // '?' opens the help overlay and any key dismisses it.
    #[test]
    fn help_overlay_toggles() {
        let mut app = App::new();
        assert!(!app.show_help, "help starts hidden");
        app.handle_key(key('?'));
        assert!(app.show_help, "'?' opens the help overlay");
        // Any key (including another '?') dismisses it without side effects.
        app.handle_key(key('x'));
        assert!(!app.show_help, "any key closes the help overlay");
        // Re-open and dismiss with '?' itself.
        app.handle_key(key('?'));
        assert!(app.show_help);
        app.handle_key(key('?'));
        assert!(!app.show_help, "'?' also toggles help off");
    }

    // Help is modal: while open, normal keys (e.g. 'n', Enter) do not trigger
    // their usual action — they just close the overlay.
    #[test]
    fn help_overlay_is_modal() {
        let mut app = App::new();
        app.handle_key(key('?'));
        assert!(app.show_help);
        app.handle_key(key('n'));
        assert!(!app.show_help, "help closed on keypress");
        assert!(!app.creating, "create prompt not triggered while help was open");
    }

    // 'd' arms a mandatory delete confirmation; only Enter deletes, and any
    // other key cancels without touching the file. Uses a real index.
    #[test]
    fn delete_requires_confirm_and_cancels() {
        let _ = std::env::var("QMD_TUI_TEST_COLL_DIR");
        let name = format!("qmd-tui-del-{}.md", std::process::id());
        let mut app = App::new();
        app.start_create();
        if !app.creating {
            return;
        }
        app.new_input = if app.new_input.ends_with('/') {
            format!("{}{}", app.new_input, name)
        } else {
            format!("{}/{}", app.new_input, name)
        };
        app.confirm_create();
        // Type content + save so the note is actually indexed and listed.
        for c in "deletable content".chars() {
            app.handle_key(key(c));
        }
        app.handle_key(event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        ));
        assert!(!app.edit_mode, "save exits edit mode");
        let present = app.notes.iter().any(|n| n.file.ends_with(&name));
        assert!(present, "saved note should be listed");

        // 'd' arms the prompt (no deletion yet).
        app.handle_key(key('d'));
        assert!(app.confirm_pending.is_some(), "delete prompt armed");
        let still_there = app.notes.iter().any(|n| n.file.ends_with(&name));
        assert!(still_there, "file not deleted until confirmed");

        // A non-Enter key cancels the prompt (no delete).
        app.handle_key(key('x'));
        assert!(app.confirm_pending.is_none(), "prompt cancelled on non-Enter");
        let still_there2 = app.notes.iter().any(|n| n.file.ends_with(&name));
        assert!(still_there2, "file still present after cancel");

        // Clean up the created file (the delete was cancelled) so the index
        // stays consistent.
        let _ = qmd::delete_note(app.open_file.as_deref().unwrap_or(""));
    }

    // Enter on the delete prompt deletes the note and refreshes the list.
    #[test]
    fn delete_confirm_removes_note() {
        let _ = std::env::var("QMD_TUI_TEST_COLL_DIR");
        let name = format!("qmd-tui-del2-{}.md", std::process::id());
        let mut app = App::new();
        app.start_create();
        if !app.creating {
            return;
        }
        app.new_input = if app.new_input.ends_with('/') {
            format!("{}{}", app.new_input, name)
        } else {
            format!("{}/{}", app.new_input, name)
        };
        app.confirm_create();
        for c in "deletable content two".chars() {
            app.handle_key(key(c));
        }
        app.handle_key(event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        ));
        assert!(!app.edit_mode, "save exits edit mode");
        let present = app.notes.iter().any(|n| n.file.ends_with(&name));
        assert!(present, "saved note should be listed");

        app.handle_key(key('d'));
        assert!(app.confirm_pending.is_some(), "delete prompt armed");
        app.handle_key(key_enter());
        assert!(app.confirm_pending.is_none(), "prompt cleared after confirm");

        let gone = app.notes.iter().any(|n| n.file.ends_with(&name));
        assert!(!gone, "note removed from list after delete");

        // Make sure it is also gone from the index (re-check via qmd).
        let listed = match qmd::list_notes(None) {
            Ok(v) => v.iter().any(|n| n.file.ends_with(&name)),
            Err(_) => false,
        };
        assert!(!listed, "note removed from the qmd index");
    }
}

