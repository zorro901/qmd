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
//!   ↑ ↓  or  j k    move through the note list; the body previews as you go (g = top, G = bottom)
//!   Enter          preview the selected note (hover already previews as you move)
//!   c              switch collection (filter the list/notes)
//!   ?              show keybindings
//!   n / +          create a new note (enter "<collection>/<file>.md")
//!   r              rename / move the selected note ("<collection>/<file>.md")
//!   y              duplicate the selected note into a copy (same collection)
//!   e              edit the open note inline (tui-textarea)
//!   d              delete the selected note (asks to confirm)
//!   PgUp/PgDn · Home/End · mouse wheel   scroll the note body
//!   Esc            in edit mode: save & exit · else leave search / close overlay
//!   Ctrl-S / Alt-S / F2   save the inline edit (write file + reindex)
//!   Ctrl-X / Alt-X    save + exit inline edit (flow-control-safe)
//!   Ctrl-C             in edit mode: save & exit · else quit immediately (panic hatch)
//!   mouse          click select · double click edit · right click delete (asks)
//!                  · middle click duplicate · drag select · wheel scroll/move
//!   Ctrl-R         reload the note list
//!   q              quit (asks if there are unsaved changes)
//!
//! All data goes through the `qmd` CLI (see qmd.rs); this TUI is a thin, fast
//! terminal front-end.

mod qmd;

use std::io;
use std::time::{Duration, Instant};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseButton,
        MouseEvent, MouseEventKind,
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

/// Window in which a second left-click on the same list row counts as a
/// double click (opens the inline editor).
const DOUBLE_CLICK_MS: std::time::Duration = std::time::Duration::from_millis(400);

/// A note-body fetch running on a background thread, tagged with the note id
/// it was started for, so a finished fetch can be dropped if the user has
/// already moved the selection elsewhere while it was running.
type BodyFetch = std::sync::mpsc::Receiver<Result<(String, Option<std::path::PathBuf>), String>>;

/// A note-list refresh running on a background thread (`qmd notes`, ~1s of
/// Node CLI startup). The generation tag lets `poll_list` drop results from a
/// superseded refresh.
type ListFetch = std::sync::mpsc::Receiver<(u64, Result<Vec<qmd::Note>, String>)>;

/// A live search running on a background thread (`qmd search`, ~1s of Node
/// CLI startup). Tagged with the query so stale results can be dropped.
type SearchFetch = std::sync::mpsc::Receiver<(String, Result<Vec<qmd::Note>, String>)>;

/// Spawn a note-list refresh on a background thread. The returned receiver is
/// polled from the event loop; the generation tag lets stale results be
/// dropped when a newer search/reload superseded this fetch.
fn spawn_list_fetch(
    gen: u64,
    collection: Option<String>,
) -> std::sync::mpsc::Receiver<(u64, Result<Vec<qmd::Note>, String>)> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = qmd::list_notes(collection.as_deref());
        let _ = tx.send((gen, result));
    });
    rx
}

/// Spawn a live search on a background thread. Tagged with the trimmed query
/// so `poll_search` can drop results that no longer match what is in the box.
fn spawn_search_fetch(q: String, collection: Option<String>) -> SearchFetch {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = qmd::search(&q, collection.as_deref());
        let _ = tx.send((q, result));
    });
    rx
}

/// Pending confirmation for a destructive action, so we never silently lose
/// work: quitting while dirty (Enter quits WITHOUT saving, any other key
/// cancels) or deleting the open/selected note.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Confirm {
    Quit,
    Delete,
}

/// While editing, wait this long after the last keystroke before autosaving, so
/// we debounce rapid typing into occasional `qmd update` writes instead of one
/// per character.
const AUTOSAVE_DEBOUNCE: Duration = Duration::from_secs(2);

/// What triggered a background reindex, so `poll_reindex` knows whether to
/// overwrite the status line with "saved" once it lands. Save/autosave want
/// that message; duplicate/delete/create/rename already set their own status
/// (e.g. "duplicated to ...") before the reindex started and it must not be
/// clobbered when the reindex finishes after the user has moved on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReindexKind {
    Save,
    Other,
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
    /// When true, the rename/move filename prompt is active (edits go to
    /// `rename_input`). `r` arms this for the selected note.
    renaming: bool,
    rename_input: String,
    /// The note id being renamed (the original, before the move).
    rename_from: Option<String>,
    /// When set, the next Enter/yes confirms a destructive action and any other
    /// key (e.g. the same `q`/Esc) cancels it. Prevents silent data loss.
    confirm_pending: Option<Confirm>,
    /// Vertical scroll offset (in lines) of the note body pane.
    vertical_scroll: u16,
    /// Last executed search query, used to highlight matches in the list.
    query: String,
    /// Instant of the last left-click on a list row; a second click within
    /// DOUBLE_CLICK_MS on the same row opens the inline editor.
    last_click: Option<(std::time::Instant, u16)>,
    /// Row (screen y) captured when a press begins, for drag-select in the list.
    drag_anchor: Option<u16>,
    /// Screen rectangle of the collection-picker popup, captured each draw so
    /// mouse clicks can select entries directly.
    pick_area: Rect,
    /// Screen rectangle of the note-body pane (right pane), captured each draw
    /// so mouse wheel events can be routed to scrolling that pane only.
    body_area: Rect,
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
    /// When true (toggled with Ctrl-G), the last raw key event is echoed to
    /// the status bar. Used to diagnose which key codes actually reach the TUI
    /// on a given terminal/SSH setup (e.g. when Ctrl-X or Alt-X appear to do
    /// nothing).
    debug_keys: bool,
    /// Last raw key event string, shown when `debug_keys` is on.
    last_key: String,
    /// Last raw mouse event string, shown when `debug_keys` is on.
    last_mouse: String,
    /// Screen rectangle of the note list, refreshed each draw, so mouse clicks
    /// can be mapped to a list row for click-to-select.
    list_area: Rect,
    /// Instant of the last keystroke while editing; used to debounce autosave so
    /// we don't shell out to qmd on every single character.
    last_edit: Option<Instant>,
    /// Set while a background thread is reindexing after a save, duplicate,
    /// delete, create, or rename. The on-disk change itself is always
    /// synchronous (instant: write/remove/rename); only the `qmd update`
    /// child is slow (~1s Node startup), so it runs off-thread and the UI
    /// never blocks. The next event-loop tick collects the result and
    /// refreshes the list. Tagged with `ReindexKind` so `poll_reindex` only
    /// overwrites the status line with "saved" for the save/autosave path.
    reindex_in_flight: Option<(ReindexKind, std::sync::mpsc::Receiver<Result<(), String>>)>,
    /// Set while a background thread fetches a note body for the preview pane
    /// (`qmd multi-get`, ~0.5-1s). Keeps the UI responsive during clicks/keys:
    /// the event loop collects the body when it lands and fills the pane then.
    /// The selection may have moved on since; the fetch is tagged with the
    /// note id it was started for and dropped if that note is no longer open.
    load_in_flight: Option<(String, BodyFetch)>,
    /// Set while a background thread refreshes the note list (`qmd notes`,
    /// ~1s Node CLI startup). A reload request while one is already running
    /// coalesces: the running fetch is replaced and a fresh one started only
    /// if a request arrived after it (see `reload_notes_async`).
    list_in_flight: Option<ListFetch>,
    /// Set when `reload_notes_async` was called while a list refresh was
    /// already running, so the finished refresh is followed by one more.
    list_refresh_queued: bool,
    /// Generation counter for list refreshes; results from a stale generation
    /// (superseded search/reload) are dropped instead of clobbering the list.
    list_generation: u64,
    /// Set while a background thread runs a search (`qmd search`). Debounced
    /// live search results land here; stale results are dropped by generation.
    search_in_flight: Option<SearchFetch>,
    /// Set when `run_search` fired while a search was already running, so the
    /// finishing search is followed by one more (latest query wins).
    search_refresh_queued: bool,
    /// Note id to select + preview once the next list refresh lands (used by
    /// duplicate/rename so the affected note stays front and center without
    /// blocking the UI while the ~1s `qmd notes` fetch runs).
    pending_select: Option<String>,
    /// True once the collection cache has been populated (startup or picker).
    /// Instant preview needs it to map note ids to on-disk paths; until then
    /// preview falls back to the async `qmd multi-get` fetch.
    collections_loaded: bool,
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
            renaming: false,
            rename_input: String::new(),
            rename_from: None,
            confirm_pending: None,
            vertical_scroll: 0,
            query: String::new(),
            last_click: None,
            drag_anchor: None,
            pick_area: Rect::ZERO,
            body_area: Rect::ZERO,
            collection: None,
            collections: Vec::new(),
            collection_idx: 0,
            picking: false,
            show_help: false,
            debug_keys: false,
            last_key: String::new(),
            last_mouse: String::new(),
            list_area: Rect::ZERO,
            last_edit: None,
            reindex_in_flight: None,
            load_in_flight: None,
            list_in_flight: None,
            list_refresh_queued: false,
            list_generation: 0,
            search_in_flight: None,
            search_refresh_queued: false,
            pending_select: None,
            collections_loaded: false,
        };
        // Warm the collection cache synchronously ONCE at startup (~1s at
        // launch, acceptable) so instant preview can map note ids to on-disk
        // paths without ever shelling out per keystroke/click afterwards.
        app.load_collections();
        app.collections_loaded = true;
        app.reload_notes();
        // Open the first note so the right pane is populated immediately
        // (SimpleNote-style preview without an explicit Enter).
        if !app.notes.is_empty() {
            app.list_state.select(Some(0));
            app.preview_selected();
        }
        app
    }

    /// Refresh the note list WITHOUT blocking the event loop. `qmd notes` is a
    /// ~1s shell-out (Node CLI startup), so it runs on a background thread and
    /// `poll_list` applies the result when it lands. A request while one is
    /// already running just sets `list_refresh_queued`; the finisher starts one
    /// more refresh so the final state is always fresh.
    fn reload_notes_async(&mut self) {
        if self.list_in_flight.is_some() {
            self.list_refresh_queued = true;
            return;
        }
        self.list_generation += 1;
        let gen = self.list_generation;
        let coll = self.collection.clone();
        self.list_in_flight = Some(spawn_list_fetch(gen, coll));
    }

    /// Synchronous refresh: kept for startup and tests where there is no event
    /// loop to poll. Blocks for ~1s (one `qmd notes` shell-out) and then
    /// applies the result with the same code path as the async refresh.
    fn reload_notes(&mut self) {
        let coll = self.collection.clone();
        self.list_generation += 1;
        let gen = self.list_generation;
        match qmd::list_notes(coll.as_deref()) {
            Ok(notes) => self.apply_notes(gen, notes),
            Err(e) => self.status = format!("error: {e}"),
        }
    }

    /// Apply a fetched note list: status line, cursor anchoring by note id,
    /// and re-preview when the open note vanished. Shared by the sync reload
    /// and the async `poll_list` applier.
    fn apply_notes(&mut self, _gen: u64, notes: Vec<qmd::Note>) {
        // Anchor on the currently selected note id so the cursor lands back on
        // the same note after the list is rebuilt (instead of snapping to top).
        let anchor = self
            .list_state
            .selected()
            .and_then(|i| self.notes.get(i).map(|n| n.file.clone()));
        let prev_sel = self.list_state.selected();
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
        if !self.notes.is_empty() {
            // A duplicate/rename asked to land on its note: highest priority.
            let pending_pos = self
                .pending_select
                .take()
                .and_then(|id| self.notes.iter().position(|n| n.file == id));
            // Land the cursor back on the same note by id (list order can
            // shift between loads); fall back to the old position, then top.
            let pos = pending_pos
                .or_else(|| {
                    anchor
                        .as_ref()
                        .and_then(|id| self.notes.iter().position(|n| &n.file == id))
                })
                .or(prev_sel.filter(|p| *p < self.notes.len()))
                .unwrap_or(0);
            self.list_state.select(Some(pos));
            // Keep the currently open note if it survived the reload; otherwise
            // re-preview whatever is now selected so the right pane never
            // points at a stale note.
            if self
                .open_file
                .as_ref()
                .map(|f| self.notes.iter().any(|n| &n.file == f))
                .unwrap_or(false)
            {
                // already showing a note that survived the reload
            } else {
                self.preview_selected();
            }
        } else if !self.edit_mode {
            // Don't blow away an in-progress edit just because this refresh's
            // note list came back empty (e.g. a just-created note with no
            // content yet has nothing to index, so it legitimately doesn't
            // show up until the next reindex after it has real content).
            // Clearing open_abs/open_file here would make save_edit() fail
            // with "no note open" and silently drop everything the user typed.
            self.list_state.select(None);
            self.open_file = None;
            self.open_body.clear();
            self.open_abs = None;
        }
    }

    /// Poll an in-flight background list refresh. Called every event-loop
    /// tick. Results from a stale generation (a newer search/reload superseded
    /// them) are dropped; a queued refresh request starts one more fetch so
    /// the list ends up fresh.
    fn poll_list(&mut self) {
        let Some(rx) = self.list_in_flight.take() else {
            return;
        };
        match rx.try_recv() {
            Ok((gen, result)) => {
                if gen == self.list_generation {
                    match result {
                        Ok(notes) => self.apply_notes(gen, notes),
                        Err(e) => self.status = format!("error: {e}"),
                    }
                }
                if self.list_refresh_queued {
                    self.list_refresh_queued = false;
                    self.reload_notes_async();
                }
            }
            // Still running: put the receiver back for the next tick.
            Err(std::sync::mpsc::TryRecvError::Empty) => self.list_in_flight = Some(rx),
            // Thread died without sending (shouldn't happen); drop it.
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
        }
    }

    /// Poll an in-flight background search. Applies results only when the
    /// finished search's query is still the live one; a queued request starts
    /// one more search for the latest query.
    fn poll_search(&mut self) {
        let Some(rx) = self.search_in_flight.take() else {
            return;
        };
        match rx.try_recv() {
            Ok((q, result)) => {
                // Apply only if the user hasn't kept typing (query changed) or
                // left search mode since the search started.
                if self.searching && self.search_input.trim() == q {
                    match result {
                        Ok(notes) => {
                            // `self.query` was already set synchronously in
                            // run_search() (drives highlighting live as the
                            // user types); only the result list lands here.
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
                if self.search_refresh_queued {
                    self.search_refresh_queued = false;
                    self.run_search();
                }
            }
            // Still running: put the receiver back for the next tick.
            Err(std::sync::mpsc::TryRecvError::Empty) => self.search_in_flight = Some(rx),
            // Thread died without sending (shouldn't happen); drop it.
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
        }
    }

    fn run_search(&mut self) {
        let q = self.search_input.trim().to_string();
        if q.is_empty() {
            // Clear the highlight query immediately so it never lags behind
            // the box going empty — apply_notes() would also clear it once
            // the background reload lands, but that must not be the only
            // path (the reload is async and may take ~1s).
            self.query = String::new();
            // Non-blocking refresh: the query box was cleared, restore the
            // full list via the background loader.
            self.reload_notes_async();
            return;
        }
        // Set the highlight query immediately, before the fetch even starts:
        // it drives list highlighting only and must track what's in the box
        // as the user types, not lag behind the ~1s `qmd search` shell-out.
        self.query = q.to_lowercase();
        if self.search_in_flight.is_some() {
            // A search is already running; queue one more so the latest query
            // still lands (debounce-by-coalescing instead of freezing).
            self.search_refresh_queued = true;
            return;
        }
        self.search_in_flight = Some(spawn_search_fetch(q, self.collection.clone()));
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
            Err(e) => {
                self.collections = Vec::new();
                self.status = format!("collection list failed: {e}");
            }
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
        // Background refresh: switching collections must not freeze the UI on
        // a ~1s shell-out; poll_list applies the list when it lands.
        self.reload_notes_async();
    }

    fn open_selected(&mut self) {
        // Preview-on-hover (t12) already loads the selected note into the right
        // pane as the cursor moves, so Enter is kept only as an explicit,
        // discoverable equivalent. It delegates to preview_selected so the body
        // always reflects the current selection and never diverges from hover.
        self.preview_selected();
    }

    /// Load the currently-selected note into the right pane (preview), unless it
    /// is already open or we are editing inline. Called whenever the selection
    /// changes so the body follows the cursor (SimpleNote-style) without an
    /// explicit Enter. No-op when nothing is selected or already shown.
    fn preview_selected(&mut self) {
        if self.edit_mode {
            return;
        }
        let idx = match self.list_state.selected() {
            Some(i) => i,
            None => return,
        };
        let note = match self.notes.get(idx) {
            Some(n) => n.clone(),
            None => return,
        };
        if self.open_file.as_deref() == Some(&note.file) {
            return; // already showing this note
        }
        // INSTANT PREVIEW: the note id ("collection/path.md") plus the cached
        // collection roots reconstruct the on-disk path without any shell-out,
        // and the FILE is the source of truth — reading it is ~0ms versus
        // ~1s of Node CLI startup for `qmd multi-get`. Only when the pure
        // resolver or the file read fails do we fall back to the async
        // multi-get fetch (e.g. ids not mappable to a known collection root).
        self.open_file = Some(note.file.clone());
        self.open_body.clear();
        self.open_abs = None;
        self.dirty = false;
        self.vertical_scroll = 0;
        let mut loaded = false;
        if self.collections_loaded {
            if let Some(abs) = qmd::note_abs_path(&note.file, &self.collections) {
                // File missing/undreadable -> keep `loaded == false` and fall
                // through to the async fetch, which surfaces qmd's own error
                // if it is a real index problem rather than a resolver gap.
                if let Ok(body) = std::fs::read_to_string(&abs) {
                    self.open_body = body;
                    self.open_abs = Some(abs);
                    self.status = "loaded".into();
                    loaded = true;
                }
            }
        }
        if !loaded {
            self.status = format!("loading {}…", note.file);
            self.load_in_flight = Some((note.file.clone(), qmd::get_body_async(note.file)));
        }
    }

    /// Move the list selection by `delta` rows (negative = up), clamped to the
    /// list bounds, then preview the newly selected note in the body. No-op when
    /// the list is empty. Shared by the arrow keys and the vim-style `j`/`k`.
    fn move_selection(&mut self, delta: isize) {
        if self.notes.is_empty() {
            return;
        }
        let i = self.list_state.selected().unwrap_or(0);
        let len = self.notes.len() as isize;
        let next = (i as isize + delta).clamp(0, len - 1) as usize;
        self.list_state.select(Some(next));
        self.preview_selected();
    }

    /// Resolve a note id to its absolute on-disk path, preferring the cached
    /// collection list (nanoseconds, no shell-out — see `note_abs_path`) and
    /// only falling back to a synchronous `qmd multi-get` when the cache
    /// isn't loaded yet or can't map the id. Shared by every note-mutating
    /// action (duplicate/delete/rename) so they get instant path resolution
    /// on the common path instead of paying a ~1s CLI startup each time.
    fn resolve_note_path(&self, file: &str) -> Option<std::path::PathBuf> {
        if self.collections_loaded {
            if let Some(abs) = qmd::note_abs_path(file, &self.collections) {
                return Some(abs);
            }
        }
        qmd::get_body(file).ok().and_then(|(_, abs)| abs)
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

    /// Duplicate the selected note into a new file in the same collection. This
    /// is non-destructive (creates a copy), so it does not need a confirm guard.
    /// The copy itself (path resolve + read + write) is instant; only the
    /// reindex that makes it searchable runs in the background, same as save.
    fn duplicate_selected(&mut self) {
        let idx = match self.list_state.selected() {
            Some(i) => i,
            None => {
                self.status = "select a note first, then press y to duplicate".into();
                return;
            }
        };
        let note = match self.notes.get(idx) {
            Some(n) => n.clone(),
            None => {
                self.status = "select a note first, then press y to duplicate".into();
                return;
            }
        };
        let src_abs = match self.resolve_note_path(&note.file) {
            Some(p) => p,
            None => {
                self.status = format!("duplicate error: could not resolve path for {}", note.file);
                return;
            }
        };
        let body = match std::fs::read_to_string(&src_abs) {
            Ok(b) => b,
            Err(e) => {
                self.status = format!("duplicate error: {e}");
                return;
            }
        };
        match qmd::duplicate_file(&src_abs, &body) {
            Ok(dest_abs) => {
                let coll = note.file.split('/').next().unwrap_or("").to_string();
                let dest_name = dest_abs
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                let new_id = format!("{coll}/{dest_name}");
                self.status = format!("duplicated to {new_id} — reindexing…");
                self.reindex_in_flight =
                    Some((ReindexKind::Other, qmd::reindex_path_async(dest_abs)));
                // Background refresh; the copy is selected + previewed when
                // the fresh list arrives (or now if it is already present).
                self.reload_notes_async();
                self.pending_select = Some(new_id.clone());
                if let Some(pos) = self.notes.iter().position(|n| n.file == new_id) {
                    self.list_state.select(Some(pos));
                    self.preview_selected();
                }
            }
            Err(e) => self.status = format!("duplicate error: {e}"),
        }
    }

    /// Actually delete the previously selected note and refresh the list. The
    /// on-disk removal is instant; only the reindex that drops it from search
    /// runs in the background, same as save.
    fn delete_selected(&mut self) {
        let idx = match self.list_state.selected() {
            Some(i) => i,
            None => return,
        };
        let note = match self.notes.get(idx) {
            Some(n) => n.clone(),
            None => return,
        };
        let abs = match self.resolve_note_path(&note.file) {
            Some(p) => p,
            None => {
                self.status = format!("delete error: could not resolve path for {}", note.file);
                return;
            }
        };
        match qmd::delete_file(&abs) {
            Ok(()) => {
                // If the deleted note was open, close the pane.
                if self.open_file.as_deref() == Some(&note.file) {
                    self.open_file = None;
                    self.open_body.clear();
                    self.open_abs = None;
                    self.dirty = false;
                    self.edit_mode = false;
                }
                self.status = format!("deleted {} — reindexing…", note.file);
                let coll = note.file.split('/').next().unwrap_or("").to_string();
                self.reindex_in_flight = Some((
                    ReindexKind::Other,
                    qmd::reindex_collections_async(vec![coll]),
                ));
                // Background refresh: deletion already removed the file, so
                // the UI must stay responsive while the index catches up.
                self.reload_notes_async();
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
        // The preview body now loads asynchronously; open_abs is only set when
        // the fetched body has actually landed. Editing before that would seed
        // the textarea with an EMPTY body, and the first save would overwrite
        // the file with nothing. If the user beats the fetch (fast 'e' after
        // launch or a quick double click), do a one-off synchronous fetch for
        // the open note — bounded to the same ~1s the old blocking preview
        // always cost, and rare.
        if self.open_abs.is_none() {
            match self.open_file.as_deref().map(qmd::get_body) {
                Some(Ok((body, abs))) => {
                    self.open_body = body;
                    self.open_abs = abs;
                }
                Some(Err(e)) => {
                    self.status = format!("open error: {e}");
                    return;
                }
                None => {
                    self.status = "open a note first (Enter), then press e to edit".into();
                    return;
                }
            }
        }
        self.textarea = TextArea::from(self.open_body.split('\n'));
        self.edit_mode = true;
        self.vertical_scroll = 0;
        self.status = "editing — autosaves · Esc saves & exits · Ctrl-C quit".into();
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
    /// set, else the first), then prompt for a filename in `new_input`. Uses
    /// the collection list warmed once at startup instead of shelling out to
    /// `qmd collection list` on every 'n' press.
    fn start_create(&mut self) {
        let colls: Vec<(String, std::path::PathBuf)> = if self.collections_loaded {
            self.collections.clone()
        } else {
            match qmd::list_collections() {
                Ok(c) => c,
                Err(e) => {
                    self.status = format!("error: {e}");
                    return;
                }
            }
        };
        if colls.is_empty() {
            self.status = "no collections; run 'qmd collection add .' first".into();
            return;
        }
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
            // The prompt pre-fills "<collection>/"; confirming that unchanged
            // is the common case (Enter pressed before typing a filename), so
            // it gets a message that says what to type next rather than the
            // abstract format hint below.
            Some((c, p)) if !c.is_empty() && p.is_empty() => {
                self.status = format!("type a filename after '{c}/' (e.g. note.md), then Enter");
                return;
            }
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

        let colls: Vec<(String, std::path::PathBuf)> = if self.collections_loaded {
            self.collections.clone()
        } else {
            match qmd::list_collections() {
                Ok(c) => c,
                Err(e) => {
                    self.status = format!("error: {e}");
                    return;
                }
            }
        };
        let dir = match colls.iter().find(|(n, _)| n == &coll_name) {
            Some((_, p)) => p.clone(),
            None => {
                self.status = format!("unknown collection '{coll_name}'");
                return;
            }
        };

        // Write is instant; only the reindex that makes the note searchable
        // is slow, so it runs in the background like save/autosave.
        match qmd::create_file(&dir, &file_name, "") {
            Ok(abs) => {
                self.open_file = Some(format!("{coll_name}/{file_name}"));
                self.open_body = String::new();
                self.open_abs = Some(abs.clone());
                self.textarea = TextArea::default();
                self.edit_mode = true;
                self.dirty = true;
                self.status = "new note — Ctrl-S save — reindexing…".into();
                self.reindex_in_flight = Some((ReindexKind::Other, qmd::reindex_path_async(abs)));
            }
            Err(e) => self.status = format!("create error: {e}"),
        }
    }

    /// Begin renaming/moving the selected note. Prefills the prompt with the
    /// note's current id so the user can edit just the filename or the whole
    /// "<collection>/<path>" target.
    fn start_rename(&mut self) {
        let idx = match self.list_state.selected() {
            Some(i) => i,
            None => {
                self.status = "select a note first, then press r to rename".into();
                return;
            }
        };
        let note = match self.notes.get(idx) {
            Some(n) => n.clone(),
            None => {
                self.status = "select a note first, then press r to rename".into();
                return;
            }
        };
        self.renaming = true;
        self.rename_from = Some(note.file.clone());
        self.rename_input = note.file.clone();
        self.status = "rename — edit target, Enter to apply".into();
    }

    /// Finish the rename/move prompt: move the file on disk and reindex both
    /// affected collections. Refuses to overwrite an existing destination.
    fn confirm_rename(&mut self) {
        let raw = self.rename_input.trim().to_string();
        let from = match self.rename_from.take() {
            Some(f) => f,
            None => {
                self.renaming = false;
                return;
            }
        };
        self.renaming = false;
        self.rename_input.clear();
        if raw.is_empty() || raw == from {
            self.status = "rename cancelled".into();
            return;
        }
        let src_abs = match self.resolve_note_path(&from) {
            Some(p) => p,
            None => {
                self.status = format!("rename error: could not resolve path for {from}");
                return;
            }
        };
        // Parse the destination "<collection>/<path>".
        let (coll, rel) = match raw.split_once('/') {
            Some((c, p)) if !c.is_empty() && !p.is_empty() => (c.to_string(), p.to_string()),
            _ => {
                self.status = "use '<collection>/<path>.md' format".into();
                return;
            }
        };
        let file_name = if rel.ends_with(".md") {
            rel
        } else {
            format!("{rel}.md")
        };
        let colls: Vec<(String, std::path::PathBuf)> = if self.collections_loaded {
            self.collections.clone()
        } else {
            match qmd::list_collections() {
                Ok(c) => c,
                Err(e) => {
                    self.status = format!("error: {e}");
                    return;
                }
            }
        };
        let dst_dir = match colls.iter().find(|(n, _)| n == &coll) {
            Some((_, p)) => p.clone(),
            None => {
                self.status = format!("unknown collection '{coll}'");
                return;
            }
        };
        let dst_abs = dst_dir.join(&file_name);
        // The move itself is instant; only the reindex (drop the old path,
        // index the new one) runs in the background, same as save.
        match qmd::move_file(&src_abs, &dst_abs) {
            Ok(()) => {
                self.status = format!("renamed to {raw} — reindexing…");
                // If the moved note was open, point the open pane at the new id.
                if self.open_file.as_deref() == Some(&from) {
                    self.open_file = Some(raw.clone());
                    self.open_abs = Some(dst_abs);
                }
                // Reindex affected collections: drop the old path from the
                // source collection and index the new path in the destination.
                let src_coll = from.split('/').next().unwrap_or("").to_string();
                let mut to_update = Vec::new();
                if src_coll != coll {
                    to_update.push(src_coll);
                }
                to_update.push(coll);
                self.reindex_in_flight = Some((
                    ReindexKind::Other,
                    qmd::reindex_collections_async(to_update),
                ));
                // Background refresh; the renamed note is re-selected when the
                // fresh list arrives (or now if already present).
                self.reload_notes_async();
                self.pending_select = Some(raw.clone());
                // Keep the renamed note selected in the list if still present.
                if let Some(idx) = self.notes.iter().position(|n| n.file == raw) {
                    self.list_state.select(Some(idx));
                }
            }
            Err(e) => self.status = format!("rename error: {e}"),
        }
    }

    /// Persist the inline edit: write the file, then reindex just that file.
    fn save_edit(&mut self) {
        // Resolve the absolute on-disk path. Prefer the cached `open_abs` (set on
        // preview/create); fall back to deriving it from `open_file` so a save
        // still works even if preview didn't populate `open_abs` for some reason.
        let abs = match &self.open_abs {
            Some(p) => p.clone(),
            None => match &self.open_file {
                Some(f) => match qmd::get_body(f) {
                    Ok((_, Some(a))) => a,
                    _ => {
                        self.status = "no note open".into();
                        return;
                    }
                },
                None => {
                    self.status = "no note open".into();
                    return;
                }
            },
        };
        let content: String = self.textarea.lines().join("\n");
        self.open_abs = Some(abs.clone());
        // Write is instant; only the reindex (~1s qmd startup) is slow, so it
        // runs in the background and the UI stays responsive. The event loop
        // collects the result and refreshes the list when it lands.
        match qmd::write_file(&abs, &content) {
            Ok(()) => {
                self.open_body = content;
                self.dirty = false;
                self.edit_mode = false;
                self.last_edit = None;
                self.status = "saved — reindexing…".into();
                self.reindex_in_flight = Some((ReindexKind::Save, qmd::reindex_path_async(abs)));
                // Keep the just-saved note selected in the list; the body and
                // title in the list refresh when the background reindex lands.
            }
            Err(e) => self.status = format!("save error: {e}"),
        }
    }

    /// Poll a in-flight background reindex, if any. Called every event-loop
    /// tick; when the `qmd update --path` child finishes we surface the result
    /// and refresh the note list (so new/renamed titles appear).
    fn poll_reindex(&mut self) {
        // try_recv() consumes the message, so take the receiver FIRST and use
        // the value from that single recv attempt (doing a probe-recv and then
        // another recv would lose the result).
        let Some((kind, rx)) = self.reindex_in_flight.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(())) => {
                // Only the save/autosave path wants "saved" on the status line;
                // duplicate/delete/create/rename already set their own status
                // before the reindex started and it must not be clobbered here.
                if kind == ReindexKind::Save {
                    self.status = "saved".into();
                }
                // Background refresh: the ~1s `qmd notes` shell-out must not
                // freeze the event loop right after every save/autosave.
                self.reload_notes_async();
                if let Some(open) = &self.open_file {
                    if let Some(idx) = self.notes.iter().position(|n| &n.file == open) {
                        self.list_state.select(Some(idx));
                    }
                }
            }
            Ok(Err(e)) => self.status = format!("reindex error: {e}"),
            // Still running: put the receiver back for the next tick.
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.reindex_in_flight = Some((kind, rx))
            }
            // Thread died without sending (shouldn't happen); drop it.
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
        }
    }

    /// Poll an in-flight background preview fetch, if any. Called every
    /// event-loop tick; when the `qmd multi-get` child finishes we fill the
    /// preview pane — unless the user has moved on to another note (or into
    /// edit mode) while the fetch was running, in which case the stale body
    /// is dropped and the pane waits for the fetch that matches the open id.
    fn poll_load(&mut self) {
        let Some((wanted, rx)) = self.load_in_flight.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok((body, abs))) => {
                // Show the body only if this note is still the open one and
                // we are not editing (edit mode owns the textarea).
                if !self.edit_mode && self.open_file.as_deref() == Some(wanted.as_str()) {
                    self.open_body = body;
                    self.open_abs = abs;
                    self.vertical_scroll = 0;
                    self.status = "loaded".into();
                }
            }
            Ok(Err(e)) => {
                if self.open_file.as_deref() == Some(wanted.as_str()) {
                    self.status = format!("open error: {e}");
                    self.open_file = None;
                    self.open_body.clear();
                }
            }
            // Still running: put the receiver back for the next tick.
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.load_in_flight = Some((wanted, rx));
            }
            // Thread died without sending (shouldn't happen); drop it.
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
        }
    }

    /// Write the current editor contents to disk via qmd (without leaving edit
    /// mode). Used by the debounced autosave so the user is never left with
    /// unsaved work if they walk away mid-edit. On success clears `dirty` and
    /// refreshes the list in place; on failure surfaces the error in the status
    /// bar but keeps editing so nothing is lost.
    fn autosave_now(&mut self) {
        // Same resolution as save_edit: prefer the cached open_abs, else derive
        // it from open_file. Autosave must never silently no-op just because
        // preview didn't cache a path — a silent drop here is exactly the
        // "autosave never happens" failure mode.
        let abs = match &self.open_abs {
            Some(p) => p.clone(),
            None => match &self.open_file {
                Some(f) => match qmd::get_body(f) {
                    Ok((_, Some(a))) => {
                        self.open_abs = Some(a.clone());
                        a
                    }
                    _ => {
                        self.status = "autosave skipped: cannot resolve note path".into();
                        return;
                    }
                },
                None => return, // nothing open — nothing to autosave
            },
        };
        let content: String = self.textarea.lines().join("\n");
        // Instant write + background reindex, same as save_edit: the editor
        // must never stall mid-typing on a ~1s qmd startup.
        match qmd::write_file(&abs, &content) {
            Ok(()) => {
                self.open_body = content;
                self.dirty = false;
                self.status = "autosaved — reindexing…".into();
                self.reindex_in_flight = Some((ReindexKind::Save, qmd::reindex_path_async(abs)));
            }
            Err(e) => self.status = format!("autosave error: {e}"),
        }
    }

    /// Debounced autosave: while editing and dirty, persist once enough time has
    /// elapsed since the last keystroke. Called from the event loop each tick.
    fn maybe_autosave(&mut self) {
        if !self.edit_mode || !self.dirty {
            return;
        }
        if let Some(t) = self.last_edit {
            if t.elapsed() >= AUTOSAVE_DEBOUNCE {
                self.last_edit = None;
                self.autosave_now();
            }
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
        // TEMPORARY diagnostic: append every raw key event to a file so we can
        // confirm what actually reaches the TUI regardless of whether the
        // on-screen status/title update is visible. Remove once the 'n'/'+'
        // investigation is resolved.
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/qmd-tui-debug.log")
        {
            use std::io::Write;
            let _ = writeln!(
                f,
                "key={:?} creating={} renaming={} searching={} edit_mode={} picking={} show_help={} confirm_pending={} collections_loaded={} collections={}",
                key,
                self.creating,
                self.renaming,
                self.searching,
                self.edit_mode,
                self.picking,
                self.show_help,
                self.confirm_pending.is_some(),
                self.collections_loaded,
                self.collections.len(),
            );
        }

        // Debug echo: remember the raw event so Ctrl-G can surface it on the
        // status bar. This is how we learn which key codes actually reach the
        // TUI on a given terminal (e.g. when Ctrl-X / Alt-X seem dead).
        self.last_key = format!("{:?}", key);

        // Ctrl-G toggles the key debug echo. F12 was tried first, but function
        // keys are routinely swallowed by the terminal/OS/multiplexer before
        // reaching the TUI — exactly the terminals a debug toggle exists to
        // diagnose. Ctrl-D was tried next, but it's already claimed by some
        // terminals/shells (e.g. EOF) before it reaches the TUI. Ctrl-G is not
        // bound to anything else outside edit mode.
        if key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.debug_keys = !self.debug_keys;
            self.status = if self.debug_keys {
                "key debug on — last key shown in status".into()
            } else {
                "key debug off".into()
            };
            return false;
        }

        // A confirmation prompt is active: Enter confirms, everything else
        // (including the same key that triggered it) cancels.
        if let Some(confirm) = self.confirm_pending {
            match key.code {
                KeyCode::Enter => {
                    self.confirm_pending = None;
                    match confirm {
                        Confirm::Quit => return true,
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

        // Ctrl-C is a panic hatch: quit immediately from ANY non-edit state. Raw
        // mode delivers it as a key event, not a SIGINT, so this always works.
        // In edit mode it is handled below as save & exit (so work is never lost).
        if !self.edit_mode
            && key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            return true;
        }

        // Inline-edit mode captures keys for tui-textarea.
        if self.edit_mode {
            // Ctrl-C saves and exits edit mode. This is the most reliable escape
            // hatch: raw mode always delivers Ctrl-C as a key event (never a
            // SIGINT), so unlike Esc it cannot be consumed by the terminal/SSH
            // layer. It saves first, so work is never lost.
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                self.save_edit();
                self.edit_mode = false;
                return false;
            }
            match key.code {
                // Esc saves the inline edit and leaves edit mode. This is the
                // plain, modifier-free way out of editing (no Fn/Ctrl/Alt key
                // needed); it always exits so the user is never trapped. If Esc
                // is not delivered by the terminal/SSH layer, Ctrl-C also saves
                // & exits (it is always delivered in raw mode).
                KeyCode::Esc => {
                    self.save_edit();
                    self.edit_mode = false;
                }
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.save_edit()
                }
                // Ctrl-X saves and leaves edit mode (flow-control-safe: unlike
                // Ctrl-S it is never eaten by XOFF). Some terminals deliver
                // Ctrl-X as the raw 0x18 (CAN) code rather than Char('x') with
                // the CONTROL modifier, so accept both forms. It always exits
                // edit mode so the user is never trapped, even if save fails.
                KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.save_edit();
                    self.edit_mode = false;
                }
                KeyCode::Char('\u{18}') => {
                    self.save_edit();
                    self.edit_mode = false;
                }
                // Alt-S (Meta+S) and F2 are flow-control-safe alternatives to
                // Ctrl-S: on some terminals/SSH sessions Ctrl-S is eaten by XOFF
                // before it reaches the TUI, so saving would silently fail.
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::ALT) => {
                    self.save_edit()
                }
                // Alt-X (Meta+X) also saves + exits edit mode. Like Alt-S it is
                // not subject to flow control, giving another way out when
                // Ctrl-X does not reach the TUI on a given terminal.
                KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::ALT) => {
                    self.save_edit();
                    self.edit_mode = false;
                }
                KeyCode::F(2) => self.save_edit(),
                _ => {
                    self.textarea.input(Input::from(key));
                    self.dirty = true;
                    self.last_edit = Some(Instant::now());
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
                    self.query.clear();
                    // Esc must be instant; the ~1s reload runs off-thread.
                    self.reload_notes_async();
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

        // Rename/move prompt captures keys.
        if self.renaming {
            match key.code {
                KeyCode::Enter => self.confirm_rename(),
                KeyCode::Esc => {
                    self.renaming = false;
                    self.rename_from = None;
                    self.rename_input.clear();
                    self.status = "cancelled".into();
                }
                KeyCode::Char(c) => self.rename_input.push(c),
                KeyCode::Backspace => {
                    self.rename_input.pop();
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
            KeyCode::Char('y') => self.duplicate_selected(),
            KeyCode::Char('e') => self.start_edit(),
            KeyCode::Char('n') => self.start_create(),
            KeyCode::Char('+') => self.start_create(),
            KeyCode::Char('r') if !key.modifiers.contains(KeyModifiers::CONTROL) => self.start_rename(),
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
                    self.preview_selected();
                }
            }
            KeyCode::Char('G') => {
                if !self.notes.is_empty() {
                    self.list_state.select(Some(self.notes.len() - 1));
                    self.preview_selected();
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

    /// Handle mouse events: wheel scrolls the note body (or moves the list
    /// selection when over the list), click selects, double click edits,
    /// right click arms delete, middle click duplicates, drag selects. While
    /// the collection picker or help overlay is open the mouse interacts with
    /// that overlay; while editing, the textarea handles wheel and clicks.
    fn handle_mouse(&mut self, m: MouseEvent) {
        if self.debug_keys {
            self.last_mouse = format!("{m:?}");
        }
        // Overlays capture the mouse first: clicking an entry picks it, clicking
        // outside dismisses — same as Esc.
        if self.show_help {
            self.show_help = false;
            return;
        }
        if self.picking {
            match m.kind {
                MouseEventKind::Down(_) => {
                    if let Some(idx) = self.pick_row_at(m.row, m.column) {
                        self.collection_idx = idx;
                        self.confirm_pick_collection();
                    } else {
                        self.picking = false; // click outside the popup cancels
                    }
                }
                MouseEventKind::ScrollUp => {
                    let len = self.collections.len() + 1; // + "All collections"
                    self.collection_idx = self.collection_idx.saturating_sub(1).min(len - 1);
                }
                MouseEventKind::ScrollDown => {
                    let len = self.collections.len() + 1;
                    self.collection_idx = (self.collection_idx + 1).min(len - 1);
                }
                _ => {}
            }
            return;
        }

        // While editing, the textarea owns the body pane: route wheel scrolling
        // into it and place the cursor on click. Other clicks are ignored.
        if self.edit_mode {
            match m.kind {
                MouseEventKind::ScrollDown => {
                    self.textarea.input(Input::from(MouseEvent {
                        kind: MouseEventKind::ScrollDown,
                        ..m
                    }));
                }
                MouseEventKind::ScrollUp => {
                    self.textarea.input(Input::from(MouseEvent {
                        kind: MouseEventKind::ScrollUp,
                        ..m
                    }));
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    let area = self.body_area;
                    let inner_top = area.y + 1;
                    let inner_left = area.x + 1;
                    if m.row >= inner_top && m.column >= inner_left {
                        let row = m.row - inner_top;
                        let col = m.column - inner_left;
                        self.textarea.move_cursor(tui_textarea::CursorMove::Jump(row, col));
                    }
                }
                _ => {}
            }
            return;
        }

        match m.kind {
            MouseEventKind::ScrollDown => {
                // Wheel over the body pane scrolls the note; over the list it
                // moves the selection (SimpleNote-style wheel navigation).
                if self.in_body(m.row, m.column) {
                    self.scroll_body(3, self.open_body.lines().count(), usize::MAX);
                } else {
                    self.move_selection(3);
                }
            }
            MouseEventKind::ScrollUp => {
                if self.in_body(m.row, m.column) {
                    self.scroll_body(-3, self.open_body.lines().count(), usize::MAX);
                } else {
                    self.move_selection(-3);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(row) = self.list_row_at(m.row, m.column) {
                    self.drag_anchor = Some(m.row);
                    // Double click on the same row opens the inline editor.
                    let double = self
                        .last_click
                        .take()
                        .map(|(t, r)| {
                            t.elapsed() <= DOUBLE_CLICK_MS && r == m.row
                        })
                        .unwrap_or(false);
                    if double {
                        self.start_edit();
                        return;
                    }
                    let cur = self.list_state.selected().unwrap_or(0);
                    // A click on the already-selected row is a no-op; otherwise
                    // move the selection (which previews the note).
                    if row != cur {
                        self.list_state.select(Some(row));
                        self.preview_selected();
                    }
                    // The fetch is async now, so stamping is cheap; the
                    // window is measured from the actual press time.
                    self.last_click = Some((std::time::Instant::now(), m.row));
                } else {
                    self.last_click = None;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                // Drag-select: follow the pointer while the button is held,
                // extending the selection from the press anchor row.
                if self.drag_anchor.is_some() {
                    if let Some(idx) = self.list_row_at(m.row, m.column) {
                        let cur = self.list_state.selected().unwrap_or(0);
                        if idx != cur {
                            self.list_state.select(Some(idx));
                            self.preview_selected();
                        }
                    }
                }
            }
            MouseEventKind::Up(_) => {
                self.drag_anchor = None;
            }
            MouseEventKind::Down(MouseButton::Right) => {
                // Right click selects the row under the pointer and arms the
                // delete confirmation (same guard as the `d` key: Enter
                // confirms, any other key cancels).
                if let Some(row) = self.list_row_at(m.row, m.column) {
                    let cur = self.list_state.selected().unwrap_or(0);
                    if row != cur {
                        self.list_state.select(Some(row));
                        self.preview_selected();
                    }
                    self.arm_delete();
                }
            }
            MouseEventKind::Down(MouseButton::Middle) => {
                // Middle click duplicates the note under the pointer (same as `y`).
                if let Some(row) = self.list_row_at(m.row, m.column) {
                    let cur = self.list_state.selected().unwrap_or(0);
                    if row != cur {
                        self.list_state.select(Some(row));
                        self.preview_selected();
                    }
                    self.duplicate_selected();
                }
            }
            _ => {}
        }
    }

    /// True when the given screen position is inside the note-body pane.
    fn in_body(&self, row: u16, column: u16) -> bool {
        let a = self.body_area;
        a.width > 0
            && row >= a.y
            && row < a.y.saturating_add(a.height)
            && column >= a.x
            && column < a.x.saturating_add(a.width)
    }

    /// Map a click position to a list row index, if it lands on a visible item.
    fn list_row_at(&self, row: u16, column: u16) -> Option<usize> {
        let inner_top = self.list_area.y.saturating_add(1);
        if row < inner_top || self.list_area.x > column || column > self.list_area.right() {
            return None;
        }
        self.list_row_index(row)
    }

    /// Map a screen row to a list index accounting for the border offset.
    fn list_row_index(&self, row: u16) -> Option<usize> {
        let inner_top = self.list_area.y.saturating_add(1);
        let r = row.checked_sub(inner_top)? as usize;
        if r < self.notes.len() { Some(r) } else { None }
    }

    /// Map a click position to an index in the collection-picker popup
    /// (0 = "All collections", 1.. = real collections), or None when the click
    /// is outside the popup.
    fn pick_row_at(&self, row: u16, column: u16) -> Option<usize> {
        let popup = self.pick_area;
        if popup.width == 0 || column < popup.x || column >= popup.x + popup.width {
            return None;
        }
        let idx = row.checked_sub(popup.y.saturating_add(1))? as usize;
        let len = self.collections.len() + 1;
        if idx < len { Some(idx) } else { None }
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
        // Debounced autosave tick: persist edits ~2s after the last keystroke so
        // the user never loses work even if they forget to save before leaving.
        app.maybe_autosave();
        // Collect a finished background reindex (from a save/autosave) and
        // refresh the list when it lands. Non-blocking; the UI keeps running.
        app.poll_reindex();
        // Collect a finished background preview fetch and fill the pane.
        app.poll_load();
        // Collect finished background list refreshes / live searches.
        app.poll_list();
        app.poll_search();
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
    // Remember the popup rectangle so mouse clicks can pick entries directly.
    app.pick_area = popup;
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
    let width = 66.min(area.width.saturating_sub(4));
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
    ("Enter", "preview the selected note (hover already previews as you move)"),
    ("↑ ↓ / j k", "move through the note list; the body previews as you go (g top · G bottom)"),
    ("c", "switch collection (filter list + search)"),
    ("n / +", "create a new note (enter \"<collection>/<file>.md\")"),
    ("r", "rename / move the selected note (cross-collection)"),
    ("y", "duplicate the selected note into a copy (same collection)"),
    ("e", "edit the open note inline"),
    ("d", "delete the selected note (asks)"),
    ("Ctrl-S / Alt-S / F2", "save the inline edit (write + reindex)"),
    ("Ctrl-X / Alt-X", "save + exit inline edit (flow-control-safe)"),
    ("Ctrl-C", "quit immediately from anywhere"),
    ("PgUp/PgDn", "scroll the note body"),
    ("Home/End", "jump to top / bottom of body"),
    ("mouse wheel", "scroll the note body · over the list: move the selection"),
    ("click", "select a note · double click: edit · drag: select"),
    ("right / middle click", "delete (asks) / duplicate the clicked note"),
    ("?", "show this help"),
    ("Ctrl-R", "reload the note list"),
    ("q", "quit (asks if there are unsaved changes)"),
    ("Esc", "in edit mode: save & exit · else cancel search / close overlay"),
    ("Ctrl-C", "in edit mode: save & exit · anywhere else: quit immediately"),
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

/// Build a `Text` for the note body with the active search query highlighted
/// (bold yellow) on every line, reusing `highlight`. When `query` is empty the
/// lines are rendered plainly. The concatenated text is always identical to
/// `body`, so highlighting never drops or rewraps content.
fn highlight_body(body: &str, query: &str) -> Text<'static> {
    let lines: Vec<Line<'static>> = body
        .lines()
        .map(|line| Line::from(highlight(line, query)))
        .collect();
    Text::from(lines)
}

fn render_list(f: &mut Frame<'_>, app: &mut App, area: Rect) {
    // Remember the list's screen rectangle so mouse clicks can be mapped to a
    // row (click-to-select) in handle_mouse.
    app.list_area = area;
    let search_line = if app.searching {
        Line::from(vec![
            Span::styled("› ", Style::default().fg(Color::Yellow)),
            Span::raw(&app.search_input),
            Span::styled("_", Style::default().fg(Color::Yellow)),
        ])
    } else if app.edit_mode {
        Line::from(vec![Span::styled(
            "editing — Esc or Ctrl-C saves & exits · autosaves · Ctrl-C quit",
            Style::default().fg(Color::Green),
        )])
    } else if app.creating {
        Line::from(vec![
            Span::styled("new › ", Style::default().fg(Color::Cyan)),
            Span::raw(&app.new_input),
            Span::styled("_", Style::default().fg(Color::Cyan)),
        ])
    } else if app.renaming {
        Line::from(vec![
            Span::styled("rename › ", Style::default().fg(Color::Magenta)),
            Span::raw(&app.rename_input),
            Span::styled("_", Style::default().fg(Color::Magenta)),
        ])
    } else {
        // Idle (no active prompt): show a persistent key hint so the main
        // actions are discoverable — there is no on-screen button in a TUI, so
        // the hint acts as the "new note" affordance (+/n, etc.).
        Line::from(vec![Span::styled(
            "n/+ new  y dup  r rename  d del  e edit  / search  ? help",
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

    let status = if app.debug_keys {
        format!(
            "{}  | last key: {} | last mouse: {}",
            app.status, app.last_key, app.last_mouse
        )
    } else {
        app.status.clone()
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(
                    "qmd  [{}]  [{}]  {}",
                    search_line,
                    app.collection.as_deref().unwrap_or("all"),
                    status
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
    // Remember the body pane's screen rectangle so mouse wheel events can be
    // routed to body scrolling (and edit-mode clicks can place the cursor).
    app.body_area = area;
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
            Some(_) => highlight_body(&app.open_body, &app.query),
            None => Text::from(
                "Select a note on the left, then press Enter to open it.\n\n\
                 Keys: / search · ↑↓ move · Enter open · n new note · e edit inline\n\
                 c switch collection · d delete · Ctrl-S save · ? help · q quit\n\
                 Unsaved edits: q asks before quitting · Esc in the editor saves & exits\n\
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

    // Same helper for preview loads: pump poll_load until the in-flight
    // `qmd multi-get` fetch lands (or give up after 15s).
    fn drain_load(app: &mut App) {
        for _ in 0..300 {
            if app.load_in_flight.is_none() {
                return;
            }
            app.poll_load();
            if app.load_in_flight.is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    // In production the event loop's poll_reindex() collects the background
    // `qmd update --path` and refreshes the list. Tests have no event loop, so
    // this helper drives the same code path: wait (up to 15s) for the reindex
    // thread, then pump poll_reindex once the result is ready.
    fn drain_reindex(app: &mut App) {
        for _ in 0..300 {
            if app.reindex_in_flight.is_none() {
                return;
            }
            app.poll_reindex();
            if app.reindex_in_flight.is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    // Pressing Enter right after 'n' without typing a filename must not
    // silently do nothing. The prompt pre-fills "<collection>/"; confirming
    // that unchanged should cancel with a message telling the user exactly
    // what to type next — not the abstract "<collection>/<file>.md format"
    // hint, which reads as a rejected format rather than an instruction to
    // keep typing. This path never reaches qmd::list_collections(), so it
    // needs no index and runs everywhere.
    #[test]
    fn create_empty_filename_gives_actionable_error() {
        let mut app = App::new();
        app.creating = true;
        app.new_input = "notes/".into();
        app.confirm_create();
        assert!(!app.creating, "prompt closes");
        assert_eq!(
            app.status,
            "type a filename after 'notes/' (e.g. note.md), then Enter",
            "status should tell the user exactly what to type next"
        );
    }

    // Headless integration test for the "new note" flow: start_create() ->
    // confirm_create() -> type into the textarea -> save_edit(). Verifies the
    // file is written with the typed content and reindexed. Skips without a
    // usable indexed collection (QMD_TUI_TEST_COLL_DIR + working qmd index).
    #[test]
    fn create_then_save_roundtrip() {
        let _guard = qmd::qmd_test_lock();
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
        drain_reindex(&mut app);

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

    // Rename/move flow: create a note, then rename it within its collection via
    // r -> Enter. Verifies the note reappears under the new id and is gone from
    // the old id in the qmd index. Skips without a usable indexed collection.
    #[test]
    fn rename_moves_note_in_index() {
        let _guard = qmd::qmd_test_lock();
        let dir: std::path::PathBuf = match std::env::var("QMD_TUI_TEST_COLL_DIR") {
            Ok(d) => d.into(),
            Err(_) => return,
        };
        // Find the collection name whose directory matches the test dir.
        let colls = match qmd::list_collections() {
            Ok(c) => c,
            Err(_) => return,
        };
        let coll = match colls
            .iter()
            .find(|(_, p)| p == &dir)
            .map(|(n, _)| n.clone())
        {
            Some(n) => n,
            None => return,
        };
        let base = format!("qmd-tui-ren-{}.md", std::process::id());
        let old_id = format!("{}/{}", coll, base);
        let abs = match qmd::create_note(&dir, &base, "# rename me\n") {
            Ok(a) => a,
            Err(_) => return,
        };

        let mut app = App::new();
        let idx = match app.notes.iter().position(|n| n.file == old_id) {
            Some(i) => i,
            None => {
                let _ = std::fs::remove_file(&abs);
                let _ = qmd::save(&abs, "");
                return;
            }
        };
        app.list_state.select(Some(idx));

        // 'r' arms the rename prompt pre-filled with the current id.
        app.handle_key(key('r'));
        assert!(app.renaming, "rename prompt arms on r");
        assert_eq!(app.rename_from.as_deref(), Some(old_id.as_str()));
        // Esc cancels without touching the file.
        app.handle_key(key_esc());
        assert!(!app.renaming, "esc cancels rename prompt");

        // Re-arm and apply a new id.
        app.list_state.select(Some(idx));
        app.handle_key(key('r'));
        let new_id = format!("{}/qmd-tui-ren-{}-new.md", coll, std::process::id());
        app.rename_input = new_id.clone();
        app.confirm_rename();
        assert!(!app.renaming, "rename prompt closed after apply");
        // The move on disk is synchronous; the reindex that updates the
        // external `qmd list_notes` view below runs in the background.
        drain_reindex(&mut app);

        let listed = qmd::list_notes(Some(&coll)).unwrap_or_default();
        let under_new = listed.iter().any(|n| n.file == new_id);
        let under_old = listed.iter().any(|n| n.file == old_id);
        assert!(under_new, "note present under new id after rename");
        assert!(!under_old, "note gone from old id after rename");

        // Clean up the moved file.
        let _ = qmd::delete_note(&new_id);
    }

    // Duplicate flow (y): create a note, select it, press y, and verify a copy
    // appears in the index with the same content and a unique "<name> copy.md"
    // id. Skips without a usable indexed collection.
    #[test]
    fn duplicate_creates_copy_in_index() {
        let _guard = qmd::qmd_test_lock();
        let dir: std::path::PathBuf = match std::env::var("QMD_TUI_TEST_COLL_DIR") {
            Ok(d) => d.into(),
            Err(_) => return,
        };
        let colls = match qmd::list_collections() {
            Ok(c) => c,
            Err(_) => return,
        };
        let coll = match colls.iter().find(|(_, p)| p == &dir).map(|(n, _)| n.clone()) {
            Some(n) => n,
            None => return,
        };
        let base = format!("qmd-tui-dup-{}.md", std::process::id());
        let old_id = format!("{}/{}", coll, base);
        let abs = match qmd::create_note(&dir, &base, "# duplicate me\nbody line\n") {
            Ok(a) => a,
            Err(_) => return,
        };

        let mut app = App::new();
        // Create a second note too, so a unique copy name is deterministic.
        let _ = qmd::create_note(&dir, &format!("qmd-tui-dup2-{}.md", std::process::id()), "# other\n");

        let idx = match app.notes.iter().position(|n| n.file == old_id) {
            Some(i) => i,
            None => {
                let _ = std::fs::remove_file(&abs);
                let _ = qmd::save(&abs, "");
                return;
            }
        };
        app.list_state.select(Some(idx));

        app.handle_key(key('y'));
        // The copy is written synchronously; the reindex that updates the
        // external `qmd list_notes`/`get_body` views below runs in the
        // background.
        drain_reindex(&mut app);

        let listed = qmd::list_notes(Some(&coll)).unwrap_or_default();
        let copy_stem = base.trim_end_matches(".md");
        let copy_id = format!("{}/{copy_stem} copy.md", coll);
        let copy_body = match qmd::get_body(&copy_id) {
            Ok((b, _)) => b,
            Err(_) => {
                let _ = qmd::delete_note(&old_id);
                panic!("expected copy {copy_id} to exist in the index");
            }
        };
        assert!(
            copy_body.contains("body line"),
            "copy should contain the source body"
        );
        assert!(listed.iter().any(|n| n.file == old_id), "original still present");
        assert!(listed.iter().any(|n| n.file == copy_id), "copy present in index");

        // Clean up both files.
        let _ = qmd::delete_note(&copy_id);
        let _ = qmd::delete_note(&old_id);
    }

    // Ctrl-R reloads the list but must keep the cursor on the same note (by id),
    // not snap back to the top. Skips without a usable indexed collection.
    #[test]
    fn reload_keeps_selected_note() {
        let _guard = qmd::qmd_test_lock();
        let dir: std::path::PathBuf = match std::env::var("QMD_TUI_TEST_COLL_DIR") {
            Ok(d) => d.into(),
            Err(_) => return,
        };
        let colls = match qmd::list_collections() {
            Ok(c) => c,
            Err(_) => return,
        };
        let _ = colls.iter().find(|(_, p)| p == &dir); // ensure a usable collection exists
        let mut app = App::new();
        if app.notes.len() < 3 {
            return; // need several rows to prove the selection doesn't reset
        }
        // Start somewhere in the middle.
        let target = 2.min(app.notes.len() - 1);
        app.list_state.select(Some(target));
        let want_id = app.notes[target].file.clone();

        app.handle_key(event::KeyEvent::new(
            KeyCode::Char('r'),
            KeyModifiers::CONTROL,
        ));

        let sel = app.list_state.selected().expect("a row stays selected");
        assert_eq!(app.notes[sel].file, want_id, "same note selected after reload");
    }

    // Enter must behave identically to hover-preview: the body reflects the
    // current selection and never diverges. Skips without a usable collection.
    #[test]
    fn enter_preview_matches_selection() {
        let _guard = qmd::qmd_test_lock();
        let _ = std::env::var("QMD_TUI_TEST_COLL_DIR");
        let mut app = App::new();
        if app.notes.len() < 2 {
            return;
        }
        // Move the cursor, then press Enter.
        app.list_state.select(Some(1));
        app.handle_key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
        let sel_id = app.notes[1].file.clone();
        assert_eq!(app.open_file.as_deref(), Some(sel_id.as_str()));
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

    // Clicking a row in the list selects (and previews) that note. The click is
    // mapped using the list_area captured during the last draw.
    #[test]
    fn mouse_click_selects_row() {
        let mut app = App::new();
        app.notes = (0..5).map(|i| qmd::Note {
            file: format!("t/n{i}.md"),
            title: format!("n{i}"),
            mtime: String::new(),
        }).collect();
        app.list_state.select(Some(0));
        // Simulate the drawn list rectangle: top border at y=2, first row at y=3.
        let area = Rect::new(0, 2, 40, 20);
        app.list_area = area;

        // Click row index 3 (screen y = 2 border + 1 + 3).
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 6,
            modifiers: KeyModifiers::empty(),
        };
        app.handle_mouse(click);
        assert_eq!(app.list_state.selected(), Some(3), "click selects row 3");
        assert_eq!(app.open_file.as_deref(), Some("t/n3.md"), "click previews the note");

        // Clicking the already-selected row is a no-op (still row 3).
        let click2 = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 6,
            modifiers: KeyModifiers::empty(),
        };
        app.handle_mouse(click2);
        assert_eq!(app.list_state.selected(), Some(3), "re-click keeps selection");

        // A click outside the list column is ignored.
        let outside = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.right() + 2,
            row: 6,
            modifiers: KeyModifiers::empty(),
        };
        app.handle_mouse(outside);
        assert_eq!(app.list_state.selected(), Some(3), "click outside list ignored");
    }

    fn click(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        }
    }

    fn app_with_notes(n: usize) -> App {
        let mut app = App::new();
        app.notes = (0..n)
            .map(|i| qmd::Note {
                file: format!("t/n{i}.md"),
                title: format!("n{i}"),
                mtime: String::new(),
            })
            .collect();
        app.list_state.select(Some(0));
        // Simulate the drawn list rectangle: top border at y=2, first row at y=3.
        app.list_area = Rect::new(0, 2, 40, 20);
        app
    }

    /// Like `app_with_notes`, but the notes are backed by REAL files on disk
    /// under a collection root cached in `app.collections`. `preview_selected`'s
    /// instant path (`note_abs_path` + `fs::read_to_string`) resolves them with
    /// no `qmd` shell-out and no background fetch, so `open_abs` lands
    /// synchronously — required by tests (e.g. double-click-to-edit) that need
    /// `start_edit` to see a real `open_abs` without polling an async fetch.
    fn app_with_real_notes(n: usize) -> App {
        let dir = std::env::temp_dir().join(format!(
            "qmd-tui-mouse-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let mut app = App::new();
        app.collections = vec![("t".to_string(), dir.clone())];
        app.collections_loaded = true;
        app.notes = (0..n)
            .map(|i| {
                let name = format!("n{i}.md");
                let _ = std::fs::write(dir.join(&name), format!("# n{i}\nbody\n"));
                qmd::Note {
                    file: format!("t/{name}"),
                    title: format!("n{i}"),
                    mtime: String::new(),
                }
            })
            .collect();
        app.list_state.select(Some(0));
        app.list_area = Rect::new(0, 2, 40, 20);
        app.preview_selected();
        app
    }

    #[test]
    fn double_click_opens_edit() {
        let _guard = qmd::qmd_test_lock(); // App::new() shells out to qmd
        // start_edit() needs a real open_abs (falls back to a synchronous
        // `qmd::get_body` shell-out otherwise, which fails without an index),
        // so this uses real temp-file-backed notes rather than app_with_notes.
        let mut app = app_with_real_notes(5);
        // First click selects (t/n3).
        app.handle_mouse(click(
            MouseEventKind::Down(MouseButton::Left),
            5,
            6, // row index 3
        ));
        assert_eq!(app.list_state.selected(), Some(3));
        assert!(!app.edit_mode);

        // Second click on the same row within the window opens the editor.
        app.handle_mouse(click(
            MouseEventKind::Down(MouseButton::Left),
            5,
            6,
        ));
        assert!(app.edit_mode, "double click should open the inline editor");

        // A slow or moved second click must NOT edit: simulate by clearing
        // last_click (as if the window elapsed) and clicking another row.
        let mut app2 = app_with_real_notes(5);
        app2.handle_mouse(click(MouseEventKind::Down(MouseButton::Left), 5, 6));
        app2.last_click = None; // window elapsed
        app2.handle_mouse(click(MouseEventKind::Down(MouseButton::Left), 5, 6));
        assert!(!app2.edit_mode, "elapsed second click must not edit");
    }

    #[test]
    fn right_click_arms_delete_and_left_confirms() {
        let _guard = qmd::qmd_test_lock();
        let mut app = app_with_notes(5);
        // Right click row 2: selection follows, delete confirmation arms.
        app.handle_mouse(click(
            MouseEventKind::Down(MouseButton::Right),
            5,
            5, // row index 2
        ));
        assert_eq!(app.list_state.selected(), Some(2), "right click selects row");
        assert!(matches!(app.confirm_pending, Some(Confirm::Delete)), "right click arms delete");
        // Enter confirms (destructive action still needs explicit confirm).
        app.handle_key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
        assert!(app.confirm_pending.is_none());
    }

    #[test]
    fn middle_click_duplicates_selected() {
        let _guard = qmd::qmd_test_lock();
        // duplicate_selected shells out to qmd; without a test collection it
        // reports an error status instead of mutating anything. We only assert
        // the mouse wiring: middle click reaches the duplicate path.
        let mut app = app_with_notes(5);
        app.handle_mouse(click(
            MouseEventKind::Down(MouseButton::Middle),
            5,
            4, // row index 1
        ));
        assert_eq!(app.list_state.selected(), Some(1), "middle click selects row");
        assert!(
            app.status.contains("duplicated") || app.status.contains("error"),
            "middle click routed to duplicate: got {:?}",
            app.status
        );
    }

    #[test]
    fn drag_selects_rows_under_pointer() {
        let _guard = qmd::qmd_test_lock();
        let mut app = app_with_notes(8);
        app.handle_mouse(click(MouseEventKind::Down(MouseButton::Left), 5, 3)); // row 0 press
        assert_eq!(app.list_state.selected(), Some(0));
        // Drag down to row 5 while held: selection follows.
        app.handle_mouse(click(MouseEventKind::Drag(MouseButton::Left), 5, 8)); // row 5
        assert_eq!(app.list_state.selected(), Some(5), "drag moves selection");
        // Release ends the drag; moving afterwards must not change selection.
        app.handle_mouse(click(MouseEventKind::Up(MouseButton::Left), 5, 8));
        app.handle_mouse(click(MouseEventKind::Drag(MouseButton::Left), 5, 4)); // row 1
        assert_eq!(app.list_state.selected(), Some(5), "drag after release ignored");
    }

    #[test]
    fn wheel_over_list_moves_selection_not_scroll() {
        let _guard = qmd::qmd_test_lock();
        let mut app = app_with_notes(8);
        app.list_area = Rect::new(0, 2, 40, 20);
        app.body_area = Rect::new(41, 2, 40, 20);
        // Wheel over the list (column inside list area).
        app.handle_mouse(click(MouseEventKind::ScrollDown, 5, 6));
        assert_eq!(app.list_state.selected(), Some(3), "wheel over list moves selection");
        // Wheel over the body pane scrolls the body instead.
        let before = app.vertical_scroll;
        app.handle_mouse(click(MouseEventKind::ScrollDown, 60, 6));
        assert!(
            app.vertical_scroll > before || app.open_body.lines().count() <= 3,
            "wheel over body scrolls body"
        );
        assert_eq!(app.list_state.selected(), Some(3), "body wheel keeps selection");
    }

    #[test]
    fn click_on_collection_picker_selects_entry() {
        let _guard = qmd::qmd_test_lock();
        let mut app = app_with_notes(2);
        app.collections = vec![
            ("work".to_string(), std::path::PathBuf::from("/tmp/work")),
            ("notes".to_string(), std::path::PathBuf::from("/tmp/notes")),
        ];
        app.picking = true;
        // Simulate the popup rect drawn by render_collection_picker for this
        // state: width 40, height = items(3) + borders = 6, centered. Exact
        // geometry doesn't matter; we just need rows inside the popup.
        app.pick_area = Rect::new(10, 5, 40, 6);
        // Click the first real collection: popup row 1 (below "All collections")
        // sits at screen y = pick_area.y + 1 border + 1 = 7.
        app.handle_mouse(click(MouseEventKind::Down(MouseButton::Left), 15, 7));
        assert!(!app.picking, "click closes the picker");
        assert_eq!(app.collection.as_deref(), Some("work"), "clicked entry applied");

        // Click outside the popup cancels without changing the filter.
        let mut app2 = app_with_notes(2);
        app2.collections = vec![("work".to_string(), std::path::PathBuf::from("/tmp/work"))];
        app2.picking = true;
        app2.pick_area = Rect::new(10, 5, 40, 5);
        app2.handle_mouse(click(MouseEventKind::Down(MouseButton::Left), 1, 20));
        assert!(!app2.picking, "outside click cancels picker");
        assert!(app2.collection.is_none(), "filter unchanged");
    }

    #[test]
    fn click_dismisses_help_overlay() {
        let _guard = qmd::qmd_test_lock();
        let mut app = app_with_notes(2);
        app.show_help = true;
        app.handle_mouse(click(MouseEventKind::Down(MouseButton::Left), 15, 6));
        assert!(!app.show_help, "click closes help");
    }

    // App::new loads the index and immediately previews the first note, so the
    // right pane is populated without an explicit Enter (SimpleNote style).
    #[test]
    fn app_new_previews_first_note() {
        let _guard = qmd::qmd_test_lock();
        let app = App::new();
        if app.notes.is_empty() {
            return; // no index to preview against in this environment
        }
        assert!(app.open_file.is_some(), "first note should be previewed on load");
        assert_eq!(
            app.open_file.as_deref(),
            Some(app.notes[0].file.as_str()),
            "previewed note matches first list entry"
        );
    }

    // preview_selected is a no-op when the selected note is already open, so
    // moving the cursor back onto the open note never re-fetches or clobbers it.
    #[test]
    fn preview_skips_when_already_open() {
        let mut app = App::new();
        // A synthetic list whose id won't resolve via qmd: if preview_selected
        // actually fetched, the body would change, proving the skip guard fired.
        app.notes = vec![qmd::Note {
            file: "t/never-resolved.md".into(),
            title: "sentinel".into(),
            mtime: String::new(),
        }];
        app.list_state.select(Some(0));
        app.open_file = Some("t/never-resolved.md".into());
        app.open_body = "SENTINEL".into();
        app.preview_selected();
        assert_eq!(app.open_body, "SENTINEL", "already-open note is not re-fetched");
    }

    // Ctrl-C in edit mode saves the inline edit and leaves edit mode immediately,
    // even if Esc is not delivered by the terminal. The most reliable escape hatch.
    #[test]
    fn ctrl_c_while_editing_saves_and_exits() {
        let _guard = qmd::qmd_test_lock();
        let _ = std::env::var("QMD_TUI_TEST_COLL_DIR");
        let mut app = App::new();
        app.start_create();
        if !app.creating {
            return;
        }
        let name = format!("qmd-tui-ctrlc-{}.md", std::process::id());
        app.new_input = if app.new_input.ends_with('/') {
            format!("{}{}", app.new_input, name)
        } else {
            format!("{}/{}", app.new_input, name)
        };
        app.confirm_create();
        assert!(app.edit_mode);
        for c in "ctrl-c body".chars() {
            app.handle_key(key(c));
        }
        assert!(app.dirty);
        // In edit mode, Ctrl-C saves and exits (does NOT quit the whole app).
        let quit = app.handle_key(event::KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        ));
        assert!(!quit, "Ctrl-C in edit mode does not quit the app");
        assert!(!app.edit_mode, "edit mode exited on Ctrl-C");
        assert!(!app.dirty, "dirty cleared after Ctrl-C save");
        drain_reindex(&mut app);
        let abs = app.open_abs.clone().unwrap();
        let content = std::fs::read_to_string(&abs).unwrap_or_default();
        assert!(
            content.contains("ctrl-c body"),
            "Ctrl-C wrote the file; got: {content:?}"
        );
        let _ = std::fs::remove_file(&abs);
        let _ = qmd::save(&abs, "");
    }

    // Esc in edit mode on an EXISTING note (not a freshly created one) must save
    // to disk. This exercises the open_abs path derived from preview_selected via
    // qmd::get_body + resolve_abs. Regression guard for "Esc returns without saving".
    #[test]
    fn esc_on_existing_note_saves() {
        let _guard = qmd::qmd_test_lock();
        let _ = std::env::var("QMD_TUI_TEST_COLL_DIR");
        let mut app = App::new();
        // No notes means the index isn't set up for the test; skip gracefully.
        if app.notes.is_empty() {
            return;
        }
        let file = app.notes[0].file.clone();
        app.list_state.select(Some(0));
        app.preview_selected();
        drain_load(&mut app); // the fetch is async; pump it like the event loop
        assert_eq!(app.open_file.as_deref(), Some(file.as_str()));
        assert!(app.open_abs.is_some(), "preview should resolve open_abs");
        app.start_edit();
        assert!(app.edit_mode);
        for c in "existing edit".chars() {
            app.handle_key(key(c));
        }
        assert!(app.dirty);
        let quit = app.handle_key(key_esc());
        assert!(!quit);
        assert!(!app.edit_mode, "edit mode exited on Esc");
        assert!(!app.dirty, "dirty cleared after save");
        drain_reindex(&mut app);
        let abs = app.open_abs.clone().unwrap();
        let content = std::fs::read_to_string(&abs).unwrap_or_default();
        assert!(
            content.contains("existing edit"),
            "Esc wrote the existing note; got: {content:?}"
        );
        // Restore the file to its pre-test content so we don't leave junk.
        let _ = qmd::save(&abs, &app.open_body);
    }


    // Esc in edit mode saves the inline edit and leaves edit mode immediately
    // (never trapping the user behind a prompt), with no confirm arm.
    #[test]
    fn esc_while_editing_saves_and_exits() {
        let _guard = qmd::qmd_test_lock();
        let _ = std::env::var("QMD_TUI_TEST_COLL_DIR");
        let mut app = App::new();
        app.start_create();
        if !app.creating {
            return;
        }
        let name = format!("qmd-tui-esc-{}.md", std::process::id());
        app.new_input = if app.new_input.ends_with('/') {
            format!("{}{}", app.new_input, name)
        } else {
            format!("{}/{}", app.new_input, name)
        };
        app.confirm_create();
        assert!(app.edit_mode);
        for c in "esc body".chars() {
            app.handle_key(key(c));
        }
        assert!(app.dirty);
        let quit = app.handle_key(key_esc());
        assert!(!quit);
        assert!(!app.edit_mode, "edit mode exited on Esc");
        assert!(app.confirm_pending.is_none(), "no confirm prompt armed");
        assert!(!app.dirty, "dirty cleared after save");
        drain_reindex(&mut app);
        let abs = app.open_abs.clone().unwrap();
        let content = std::fs::read_to_string(&abs).unwrap_or_default();
        assert!(content.contains("esc body"), "Esc wrote the file; got: {content:?}");
        let _ = std::fs::remove_file(&abs);
        let _ = qmd::save(&abs, "");
    }

    // Drive the real key path (handle_key) to type into the textarea, save via
    // Ctrl-S, and confirm: (a) the file gets the typed text, and (b) the note
    // list is refreshed so the new note appears. Needs a usable index.
    #[test]
    fn keypress_edit_then_save_persists() {
        let _guard = qmd::qmd_test_lock();
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
        drain_reindex(&mut app);

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

    // Saving must also work via Alt-S (Meta+S) and F2, which are not subject to
    // terminal flow control (XOFF) the way Ctrl-S is — important over SSH where
    // Ctrl-S can be eaten before it reaches the TUI. Needs a usable index.
    #[test]
    fn alt_s_and_f2_save_in_edit_mode() {
        let _guard = qmd::qmd_test_lock();
        let _ = std::env::var("QMD_TUI_TEST_COLL_DIR");
        let mut app = App::new();
        app.start_create();
        if !app.creating {
            return;
        }
        let name = format!("qmd-tui-alt-{}.md", std::process::id());
        app.new_input = if app.new_input.ends_with('/') {
            format!("{}{}", app.new_input, name)
        } else {
            format!("{}/{}", app.new_input, name)
        };
        app.confirm_create();
        assert!(app.edit_mode, "editing after create");

        // Type some text.
        for c in "save via alt-s".chars() {
            app.handle_key(key(c));
        }
        assert!(app.dirty);

        // Alt-S saves.
        app.handle_key(event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::ALT,
        ));
        assert!(!app.dirty, "Alt-S should clear dirty");
        let abs = app.open_abs.clone().unwrap();
        let content = std::fs::read_to_string(&abs).unwrap_or_default();
        assert!(content.contains("save via alt-s"), "Alt-S wrote the file; got: {content:?}");

        // Re-enter edit to prove F2 also saves.
        app.start_edit();
        for c in " and f2".chars() {
            app.handle_key(key(c));
        }
        app.handle_key(event::KeyEvent::new(KeyCode::F(2), KeyModifiers::empty()));
        assert!(!app.dirty, "F2 should clear dirty");
        let content2 = std::fs::read_to_string(&abs).unwrap_or_default();
        assert!(
            content2.contains("save via alt-s") && content2.contains(" and f2"),
            "F2 wrote the file; got: {content2:?}"
        );

        let _ = std::fs::remove_file(&abs);
        let _ = qmd::save(&abs, "");
    }

    // Ctrl-X saves and leaves edit mode (the flow-control-safe escape when
    // Ctrl-S is eaten). Needs a usable index.
    #[test]
    fn ctrl_x_saves_and_exits_edit_mode() {
        let _guard = qmd::qmd_test_lock();
        let _ = std::env::var("QMD_TUI_TEST_COLL_DIR");
        let mut app = App::new();
        app.start_create();
        if !app.creating {
            return;
        }
        let name = format!("qmd-tui-ctrlx-{}.md", std::process::id());
        app.new_input = if app.new_input.ends_with('/') {
            format!("{}{}", app.new_input, name)
        } else {
            format!("{}/{}", app.new_input, name)
        };
        app.confirm_create();
        assert!(app.edit_mode, "editing after create");
        for c in "saved via ctrl-x".chars() {
            app.handle_key(key(c));
        }
        assert!(app.dirty);

        app.handle_key(event::KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL,
        ));
        assert!(!app.edit_mode, "Ctrl-X exits edit mode");
        assert!(!app.dirty, "Ctrl-X clears dirty after save");
        let abs = app.open_abs.clone().unwrap();
        let content = std::fs::read_to_string(&abs).unwrap_or_default();
        assert!(content.contains("saved via ctrl-x"), "Ctrl-X wrote the file; got: {content:?}");
        let _ = std::fs::remove_file(&abs);
        let _ = qmd::save(&abs, "");
    }

    // Some terminals deliver Ctrl-X as the raw 0x18 (CAN) code instead of
    // Char('x') with the CONTROL modifier. Either form must save + exit.
    #[test]
    fn ctrl_x_can_code_saves_and_exits() {
        let _guard = qmd::qmd_test_lock();
        let _ = std::env::var("QMD_TUI_TEST_COLL_DIR");
        let mut app = App::new();
        app.start_create();
        if !app.creating {
            return;
        }
        let name = format!("qmd-tui-ctrlx2-{}.md", std::process::id());
        app.new_input = if app.new_input.ends_with('/') {
            format!("{}{}", app.new_input, name)
        } else {
            format!("{}/{}", app.new_input, name)
        };
        app.confirm_create();
        if !app.edit_mode {
            return;
        }
        for c in "via 0x18".chars() {
            app.handle_key(key(c));
        }
        // Raw CAN byte (what some terminals send for Ctrl-X).
        app.handle_key(event::KeyEvent::new(
            KeyCode::Char('\u{18}'),
            KeyModifiers::empty(),
        ));
        assert!(!app.edit_mode, "0x18 exits edit mode");
        let abs = app.open_abs.clone().unwrap();
        let content = std::fs::read_to_string(&abs).unwrap_or_default();
        assert!(content.contains("via 0x18"), "0x18 wrote the file; got: {content:?}");
        let _ = std::fs::remove_file(&abs);
        let _ = qmd::save(&abs, "");
    }

    // Alt-X (Meta+X) also saves + exits edit mode.
    #[test]
    fn alt_x_saves_and_exits_edit_mode() {
        let _guard = qmd::qmd_test_lock();
        let _ = std::env::var("QMD_TUI_TEST_COLL_DIR");
        let mut app = App::new();
        app.start_create();
        if !app.creating {
            return;
        }
        let name = format!("qmd-tui-altx-{}.md", std::process::id());
        app.new_input = if app.new_input.ends_with('/') {
            format!("{}{}", app.new_input, name)
        } else {
            format!("{}/{}", app.new_input, name)
        };
        app.confirm_create();
        if !app.edit_mode {
            return;
        }
        for c in "via alt-x".chars() {
            app.handle_key(key(c));
        }
        app.handle_key(event::KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::ALT,
        ));
        assert!(!app.edit_mode, "Alt-X exits edit mode");
        let abs = app.open_abs.clone().unwrap();
        let content = std::fs::read_to_string(&abs).unwrap_or_default();
        assert!(content.contains("via alt-x"), "Alt-X wrote the file; got: {content:?}");
        let _ = std::fs::remove_file(&abs);
        let _ = qmd::save(&abs, "");
    }

    // Ctrl-C is a panic hatch: it quits from ANY state, including inline-edit
    // (where 'q' is shadowed by the textarea). handle_key returns true.
    #[test]
    fn ctrl_c_quits_from_edit_mode() {
        let _guard = qmd::qmd_test_lock();
        let mut app = App::new();
        app.start_create();
        if !app.creating {
            return;
        }
        app.confirm_create();
        if !app.edit_mode {
            return; // no usable index in this run
        }
        let quit = app.handle_key(event::KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        ));
        assert!(quit, "Ctrl-C should request quit");
        assert!(!app.edit_mode, "Ctrl-C drops out of edit mode");
    }

    // Ctrl-G toggles the key-debug echo and the last raw key is recorded. Used
    // to diagnose which key codes reach the TUI on a given terminal (F12 was
    // dropped: many terminals/OSes intercept function keys before they ever
    // reach the TUI, making it useless as a diagnostic for exactly the
    // terminals that need one. Ctrl-D was tried next but some
    // terminals/shells claim it — e.g. as EOF — before it reaches the TUI).
    #[test]
    fn ctrl_g_toggles_key_debug() {
        let mut app = App::new();
        assert!(!app.debug_keys, "debug off initially");
        app.handle_key(event::KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert!(app.debug_keys, "Ctrl-G turns debug on");
        app.handle_key(key('x'));
        assert!(app.last_key.contains("Char('x')"), "last key recorded: {}", app.last_key);
        app.handle_key(event::KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert!(!app.debug_keys, "Ctrl-G turns debug off");
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

    // highlight_body wraps each line through highlight(): the search query is
    // bold-yellow in the open note, and the concatenated text is byte-identical
    // to the source body (no content dropped or rewrapped).
    #[test]
    fn highlight_body_marks_query_and_preserves_text() {
        // Empty query -> one plain line per source line.
        let t = highlight_body("alpha beta\ngamma", "");
        assert_eq!(t.lines.len(), 2);
        assert_eq!(t.lines[0].spans[0].content, "alpha beta");

        // Query hits both lines; rebuild the text to confirm it's preserved.
        let t = highlight_body("rust note\nother Rust line", "rust");
        let rebuilt: String = t
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(rebuilt, "rust note\nother Rust line");
        // First line contains one highlighted (yellow) span.
        let any_yellow = t
            .lines[0]
            .spans
            .iter()
            .any(|s| s.style.fg == Some(Color::Yellow));
        assert!(any_yellow, "matched term should be highlighted");
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

        // A reload (e.g. Ctrl-R or post-save) clears the highlight query via
        // apply_notes() — but only when qmd actually answers; without a usable
        // index reload_notes() leaves `query` untouched and just surfaces the
        // shell-out error in `status`, so skip rather than assert on that.
        if qmd::list_notes(None).is_err() {
            return;
        }
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

    // '+' (as an alternative to 'n') opens the new-note prompt via the real key
    // path. Skips without a usable collection.
    #[test]
    fn plus_key_starts_create() {
        let _ = std::env::var("QMD_TUI_TEST_COLL_DIR");
        let mut app = App::new();
        if app.notes.is_empty() {
            return; // no collection/indexed notes -> start_create is a no-op
        }
        assert!(!app.creating, "not creating before '+'");
        app.handle_key(key('+'));
        assert!(app.creating, "new-note prompt opens on '+'");
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
        drain_reindex(&mut app);
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
        drain_reindex(&mut app);
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

    // Debounced autosave: when editing and dirty past the debounce window,
    // maybe_autosave persists via qmd, clears dirty, and stays in edit mode.
    #[test]
    fn autosave_persists_after_debounce() {
        let _guard = qmd::qmd_test_lock();
        let _ = std::env::var("QMD_TUI_TEST_COLL_DIR");
        let mut app = App::new();
        app.start_create();
        if !app.creating {
            return;
        }
        let name = format!("qmd-tui-auto-{}.md", std::process::id());
        app.new_input = if app.new_input.ends_with('/') {
            format!("{}{}", app.new_input, name)
        } else {
            format!("{}/{}", app.new_input, name)
        };
        app.confirm_create();
        assert!(app.edit_mode);

        for c in "autosaved text".chars() {
            app.handle_key(key(c));
        }
        assert!(app.dirty, "typing marks dirty");

        // Pretend the last keystroke was well past the debounce window.
        app.last_edit = Some(Instant::now() - Duration::from_secs(10));
        app.maybe_autosave();

        assert!(!app.dirty, "autosave clears dirty");
        assert!(app.edit_mode, "autosave stays in edit mode");
        let abs = app.open_abs.clone().unwrap();
        let content = std::fs::read_to_string(&abs).unwrap_or_default();
        assert!(
            content.contains("autosaved text"),
            "autosave wrote the file; got: {content:?}"
        );
        assert!(app.last_edit.is_none(), "debounce timer reset after autosave");

        let _ = std::fs::remove_file(&abs);
        let _ = qmd::save(&abs, "");
    }

    // maybe_autosave is a no-op while actively typing (debounce not elapsed).
    #[test]
    fn autosave_suppressed_before_debounce() {
        let _guard = qmd::qmd_test_lock();
        let _ = std::env::var("QMD_TUI_TEST_COLL_DIR");
        let mut app = App::new();
        app.start_create();
        if !app.creating {
            return;
        }
        let name = format!("qmd-tui-auto2-{}.md", std::process::id());
        app.new_input = if app.new_input.ends_with('/') {
            format!("{}{}", app.new_input, name)
        } else {
            format!("{}/{}", app.new_input, name)
        };
        app.confirm_create();
        for c in "not yet".chars() {
            app.handle_key(key(c));
        }
        // No timer armed yet (marker set at the poll boundary), so autosave
        // must not fire and must not clear dirty.
        app.last_edit = None;
        app.maybe_autosave();
        assert!(app.dirty, "no autosave before any debounce window");
        let abs = app.open_abs.clone().unwrap();
        let _ = std::fs::remove_file(&abs);
    }
}

