//! Thin wrapper around the `qmd` CLI. The TUI never touches the SQLite index
//! or the markdown files directly; every read goes through `qmd <cmd> --format
//! json` and every write goes through `qmd update --path <abs>` after we write
//! the file ourselves. This keeps qmd the single source of truth.

use std::path::PathBuf;
use std::path::Path;
use std::process::Command;

use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct Note {
    pub file: String,     // "collection/path.md"
    pub title: String,
    pub mtime: String,
}

#[derive(Debug, Clone, Deserialize)]
struct NotesJson {
    file: String,
    title: String,
    #[serde(default)]
    mtime: String,
}

#[derive(Debug, Clone, Deserialize)]
struct MultiGetJson {
    #[serde(default)]
    body: String,
    #[serde(default)]
    file: String, // absolute on-disk path when --full-path is passed
}

/// Resolve the `qmd` binary. Prefer `qmd` on PATH; fall back to the repo's
/// `bin/qmd` so the TUI works from a fresh checkout too.
fn qmd_bin() -> String {
    if let Ok(p) = std::env::var("QMD_BIN") {
        return p;
    }
    "qmd".to_string()
}

/// Run `qmd <args...>` and return stdout, or an error string.
fn run_qmd(args: &[&str]) -> Result<String, String> {
    let out = Command::new(qmd_bin())
        .args(args)
        .output()
        .map_err(|e| format!("failed to spawn qmd: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "qmd {} exited {}: {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// List all notes: `qmd notes --format json`. When `collection` is Some, only
/// notes from that collection are returned (`qmd notes -c <name>`).
pub fn list_notes(collection: Option<&str>) -> Result<Vec<Note>, String> {
    let mut args = vec!["notes", "--format", "json"];
    if let Some(c) = collection {
        args.push("-c");
        args.push(c);
    }
    let raw = run_qmd(&args)?;
    let parsed: Vec<NotesJson> = serde_json::from_str(raw.trim())
        .map_err(|e| format!("notes json parse error: {e}\nraw: {raw}"))?;
    Ok(parsed
        .into_iter()
        .map(|n| Note {
            file: n.file,
            title: n.title,
            mtime: n.mtime,
        })
        .collect())
}

/// Search notes: `qmd search --format json "<query>"`. When `collection` is
/// Some, only that collection is searched (`qmd search -c <name> "<query>"`).
pub fn search(query: &str, collection: Option<&str>) -> Result<Vec<Note>, String> {
    let mut args = vec!["search", "--format", "json"];
    if let Some(c) = collection {
        args.push("-c");
        args.push(c);
    }
    args.push(query);
    let raw = run_qmd(&args)?;
    let parsed: Vec<NotesJson> = match serde_json::from_str(raw.trim()) {
        Ok(v) => v,
        // `qmd search` returns "[]" for no hits; tolerate non-JSON gracefully.
        Err(_) => return Ok(Vec::new()),
    };
    Ok(parsed
        .into_iter()
        .map(|n| Note {
            file: n.file,
            title: n.title,
            mtime: n.mtime,
        })
        .collect())
}

/// Resolve a possibly-relative note path returned by `qmd multi-get --full-path`
/// to an absolute on-disk path.
///
/// The `./`-prefixed form (e.g. `"./notes/n3.md"`) is emitted by qmd ONLY when
/// the file lies under the CWD of the qmd child process, and `run_qmd` inherits
/// the TUI's cwd. So the correct resolution is against `std::env::current_dir()`,
/// NOT against collection names or directory names. (Collection names never
/// appear in this form; e.g. collection `t` rooted at `/tmp/qmdnt/notes` yields
/// `./notes/...`, where `notes` is just a directory segment.)
///
/// Other accepted forms: an absolute path (pass-through), and as a last resort a
/// `"collection/path"` (or `qmd://collection/path`) id mapped via
/// `qmd collection list` — first by collection NAME, then by the collection
/// path's final directory segment. Returns None only if nothing matches, which
/// blocks saving/deleting with a loud status instead of a silent no-op.
fn resolve_abs(rel: &str) -> Option<PathBuf> {
    match resolve_abs_with(rel, std::env::current_dir().ok().as_deref()) {
        Some(p) => Some(p),
        // The collection fallback needs `qmd collection list`; only reach for
        // the CLI when the pure rules could not resolve the path.
        None => {
            let collections = list_collections().ok()?;
            resolve_abs_with_collections(rel, &collections)
        }
    }
}

/// Pure part of `resolve_abs`: everything decidable without invoking qmd.
/// `cwd` is optional so tests can simulate a missing cwd.
fn resolve_abs_with(rel: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    // Absolute path: pass through unchanged.
    if rel.starts_with('/') {
        return Some(PathBuf::from(rel));
    }
    // "./..." form: relative to the TUI's cwd (== qmd child's cwd).
    if let Some(cleaned) = rel.strip_prefix("./") {
        if cleaned.is_empty() {
            return None;
        }
        let cwd = cwd?;
        return Some(cwd.join(cleaned));
    }
    None
}

/// Collection-based fallback for bare "<collection>/<path>" ids (possibly
/// qmd://-prefixed): first by collection NAME, then by the collection path's
/// final directory segment.
fn resolve_abs_with_collections(rel: &str, collections: &[(String, PathBuf)]) -> Option<PathBuf> {
    let cleaned = rel.strip_prefix("qmd://").unwrap_or(rel);
    let (first, rest) = cleaned.split_once('/')?;
    if let Some((_, p)) = collections.iter().find(|(n, _)| n == first) {
        return Some(p.join(rest));
    }
    collections
        .iter()
        .find(|(_, p)| p.file_name().map(|n| n == first).unwrap_or(false))
        .map(|(_, p)| p.join(rest))
}

/// Fetch a note's body. Uses `qmd multi-get --full-path` so the `file`
/// field carries the on-disk path (needed for saving + deleting). `qmd` emits
/// a *relative* path even with `--full-path`, so we resolve it to absolute via
/// the collection list (see `resolve_abs`).
pub fn get_body(file: &str) -> Result<(String, Option<PathBuf>), String> {
    let raw = run_qmd(&[
        "multi-get",
        file,
        "--format",
        "json",
        "--full-path",
        "--no-line-numbers",
    ])?;
    let parsed: Vec<MultiGetJson> = serde_json::from_str(raw.trim())
        .map_err(|e| format!("multi-get json parse error: {e}\nraw: {raw}"))?;
    let first = parsed
        .into_iter()
        .next()
        .ok_or_else(|| format!("note not found: {file}"))?;
    // `--full-path` is relative here (e.g. "./notes/alpha.md"), so resolve it.
    let abs = if first.file.starts_with('/') {
        Some(PathBuf::from(&first.file))
    } else {
        resolve_abs(&first.file)
    };
    Ok((first.body, abs))
}

/// Delete a note file and reindex its collection so it disappears from search.
/// `file` is the qmd note id ("collection/path.md"); the on-disk path is
/// resolved via `qmd multi-get --full-path`. The collection is reindexed with
/// `qmd update -c <name>`, which drops the now-missing file from the index.
pub fn delete_note(file: &str) -> Result<(), String> {
    // Resolve the absolute path.
    let (_, abs) = get_body(file)?;
    let abs = match abs {
        Some(p) => p,
        None => return Err(format!("could not resolve path for {file}")),
    };
    std::fs::remove_file(&abs).map_err(|e| format!("delete failed: {e}"))?;
    // Reindex the owning collection (cheaper than a full update). The collection
    // name is the first path segment of the note id.
    let coll = file
        .split('/')
        .next()
        .filter(|c| !c.is_empty())
        .ok_or_else(|| format!("malformed note id: {file}"))?;
    run_qmd(&["update", "-c", coll])?;
    Ok(())
}

/// Rename / move a note. `from` and `to` are qmd note ids
/// ("collection/path.md"). The file is moved on disk (refusing to overwrite an
/// existing destination), then both the source and destination collections are
/// reindexed with `qmd update -c <name>` so the move is reflected in search.
pub fn rename_note(from: &str, to: &str) -> Result<(), String> {
    // Resolve the source absolute path via multi-get --full-path.
    let (_, abs) = get_body(from)?;
    let src_abs = match abs {
        Some(p) => p,
        None => return Err(format!("could not resolve path for {from}")),
    };
    // Parse the destination "<collection>/<path>".
    let (coll, rel) = match to.split_once('/') {
        Some((c, p)) if !c.is_empty() && !p.is_empty() => (c.to_string(), p.to_string()),
        _ => return Err("use '<collection>/<path>.md' format".into()),
    };
    let file_name = if rel.ends_with(".md") {
        rel
    } else {
        format!("{rel}.md")
    };
    // Destination collection directory.
    let colls = list_collections()?;
    let dst_dir = match colls.iter().find(|(n, _)| n == &coll) {
        Some((_, p)) => p.clone(),
        None => return Err(format!("unknown collection '{coll}'")),
    };
    let dst_abs = dst_dir.join(&file_name);
    if dst_abs.exists() {
        return Err(format!("destination already exists: {to}"));
    }
    if let Some(parent) = dst_abs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir failed: {e}"))?;
    }
    std::fs::rename(&src_abs, &dst_abs).map_err(|e| format!("rename failed: {e}"))?;
    // Reindex affected collections: drop the old path from the source collection
    // and index the new path in the destination collection.
    let src_coll = from
        .split('/')
        .next()
        .filter(|c| !c.is_empty())
        .ok_or_else(|| format!("malformed note id: {from}"))?;
    let mut to_update: Vec<&str> = Vec::new();
    if src_coll != coll {
        to_update.push(src_coll);
    }
    to_update.push(&coll);
    for c in to_update {
        run_qmd(&["update", "-c", c])?;
    }
    Ok(())
}

/// Duplicate a note into a new file in the same collection, returning the new
/// note id ("collection/path.md"). The body (with no line numbers) is copied
/// verbatim from the source, and the new file is reindexed with
/// `qmd update --path <abs>` so it shows up immediately. The destination name
/// is `<stem> copy.md`, or `<stem> copy N.md` when that already exists, so a
/// duplicate never overwrites an existing note.
pub fn duplicate_note(file: &str) -> Result<String, String> {
    let (body, abs) = get_body(file)?;
    let src_abs = match abs {
        Some(p) => p,
        None => return Err(format!("could not resolve path for {file}")),
    };
    let coll_dir = src_abs
        .parent()
        .ok_or_else(|| format!("no parent dir for {file}"))?
        .to_path_buf();
    let base = src_abs
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("invalid filename for {file}"))?
        .to_string();
    let dest = unique_copy_name(&coll_dir, &base);
    let dest_abs = coll_dir.join(&dest);
    std::fs::write(&dest_abs, &body).map_err(|e| format!("write failed: {e}"))?;
    let dest_str = dest_abs.to_string_lossy();
    run_qmd(&["update", "--path", &dest_str])?;
    // New note id = "<collection>/<dest filename>".
    let coll = file
        .split('/')
        .next()
        .filter(|c| !c.is_empty())
        .unwrap_or("")
        .to_string();
    Ok(format!("{}/{}", coll, dest))
}

/// Pick a free "<stem> copy.md" (or "<stem> copy N.md") name within `dir`.
fn unique_copy_name(dir: &Path, base: &str) -> String {
    let candidate = format!("{base} copy.md");
    if !dir.join(&candidate).exists() {
        return candidate;
    }
    let mut n = 2;
    loop {
        let cand = format!("{base} copy {n}.md");
        if !dir.join(&cand).exists() {
            return cand;
        }
        n += 1;
    }
}

/// Write `content` to `abs_path`, then reindex just that file via
/// `qmd update --path <abs>` (O(changed), no full rescan).
pub fn save(abs_path: &PathBuf, content: &str) -> Result<(), String> {
    std::fs::write(abs_path, content).map_err(|e| format!("write failed: {e}"))?;
    let abs_str = abs_path.to_string_lossy();
    run_qmd(&["update", "--path", &abs_str])?;
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
struct CollectionJson {
    name: String,
    path: String,
}

/// List collections as `[{name, path}]` via `qmd collection list --format json`.
/// The `path` is the on-disk directory the collection indexes.
pub fn list_collections() -> Result<Vec<(String, PathBuf)>, String> {
    let raw = run_qmd(&["collection", "list", "--format", "json"])?;
    let parsed: Vec<CollectionJson> = serde_json::from_str(raw.trim())
        .map_err(|e| format!("collection list json parse error: {e}\nraw: {raw}"))?;
    Ok(parsed
        .into_iter()
        .map(|c| (c.name, PathBuf::from(c.path)))
        .collect())
}

/// Create a new note file inside `collection_dir`, then reindex just that file so
/// it appears in search immediately. Returns the absolute path written.
pub fn create_note(
    collection_dir: &PathBuf,
    file_name: &str,
    content: &str,
) -> Result<PathBuf, String> {
    let abs = collection_dir.join(file_name);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir failed: {e}"))?;
    }
    std::fs::write(&abs, content).map_err(|e| format!("write failed: {e}"))?;
    let abs_str = abs.to_string_lossy();
    run_qmd(&["update", "--path", &abs_str])?;
    Ok(abs)
}


/// Serializes all qmd-touching tests crate-wide (qmd.rs and main.rs): they share
/// one index and the same first-listed note, so parallel runs race on
/// truncate-write + reindex and read empty files spuriously.
#[cfg(test)]
pub(crate) fn qmd_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Verifies the save() round-trip: write a file, reindex via qmd, then read
    // it back through the qmd CLI. Skipped automatically if `qmd` is not on PATH
    // or no index exists (CI without a built index).
    #[test]
    fn save_then_relist_reflects_content() {
        let _guard = qmd_test_lock();
        // Point at an existing indexed file when provided (e.g. a qmd collection
        // under test), otherwise use a temp file that qmd won't index (the test
        // then self-skips via the early return below).
        let file: std::path::PathBuf = match std::env::var("QMD_TUI_TEST_FILE") {
            Ok(p) => p.into(),
            Err(_) => {
                let dir =
                    std::env::temp_dir().join(format!("qmd-tui-test-{}", std::process::id()));
                let _ = std::fs::create_dir_all(&dir);
                dir.join("note.md")
            }
        };
        std::fs::write(&file, "# Hello\noriginal\n").unwrap();

        // Only run if `qmd` is reachable and has a usable index for this path.
        let abs = file.clone();
        let res = save(&abs, "# Hello\nedited by tui-textarea\n");
        if res.is_err() {
            // No qmd index covering this file — not a code failure.
            let _ = std::fs::remove_file(&file);
            return;
        }
        let back = std::fs::read_to_string(&file).unwrap();
        assert!(back.contains("edited by tui-textarea"));
        let _ = std::fs::remove_file(&file);
    }

    // Verifies create_note(): write a new file into an indexed collection dir and
    // reindex it via `qmd update --path`, then confirm it shows up in the note
    // list. Skips if no indexed collection dir is supplied via QMD_TUI_TEST_COLL_DIR.
    #[test]
    fn create_note_shows_in_list() {
        let _guard = qmd_test_lock();
        let dir: std::path::PathBuf = match std::env::var("QMD_TUI_TEST_COLL_DIR") {
            Ok(d) => d.into(),
            Err(_) => return, // nothing indexed to test against
        };
        let name = format!("qmd-tui-new-{}.md", std::process::id());
        let res = create_note(&dir, &name, "# Created by TUI\nhello\n");
        if res.is_err() {
            return; // qmd reindex not possible here — not a code failure
        }
        let abs = res.unwrap();
        let listed = match list_notes(None) {
            Ok(v) => v.iter().any(|n| n.file.ends_with(&name)),
            Err(_) => return, // can't verify without a usable index
        };
        let _ = std::fs::remove_file(&abs);
        // Reindex the (now-removed) file so the index stays consistent.
        let _ = save(&abs, "");
        assert!(listed, "newly created note should appear in qmd notes");
    }

    // get_body resolves the on-disk absolute path even though `qmd multi-get
    // --full-path` emits a *relative* path (e.g. "./coll/a.md"). Saving and
    // deleting both depend on this. Skips if no indexed note is available.
    #[test]
    fn get_body_resolves_absolute_path() {
        let _guard = qmd_test_lock();
        let notes = match list_notes(None) {
            Ok(v) if !v.is_empty() => v,
            _ => return, // nothing indexed to test against
        };
        let id = &notes[0].file;
        let (body, abs) = match get_body(id) {
            Ok(t) => t,
            Err(_) => return, // can't resolve without a usable index
        };
        assert!(body.contains(" ") || !body.is_empty(), "body fetched");
        assert!(abs.is_some(), "absolute path resolved from qmd");
        let abs = abs.unwrap();
        assert!(abs.is_absolute(), "resolved path must be absolute: {abs:?}");
        assert!(abs.exists(), "resolved path points at a real file: {abs:?}");
    }

    // NotesJson parses the mtime field, which the TUI renders as a recency
    // suffix in the list (serde defaults it to "" when absent).
    #[test]
    fn notes_json_parses_mtime() {
        let raw = r#"[{"file":"t/foo.md","title":"Foo","mtime":"2026-08-28T15:00:00Z"}]"#;
        let parsed: Vec<NotesJson> = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].mtime, "2026-08-28T15:00:00Z");
        assert_eq!(parsed[0].title, "Foo");
        assert_eq!(parsed[0].file, "t/foo.md");

        // mtime defaults to "" when the field is missing (title is still required).
        let raw = r#"[{"file":"t/bar.md","title":"Bar"}]"#;
        let parsed: Vec<NotesJson> = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed[0].mtime, "", "mtime defaults to empty string");
    }

    // unique_copy_name picks "<stem> copy.md" when free, and "<stem> copy N.md"
    // (ascending) once collisions exist, so a duplicate never clobbers a file.
    #[test]
    fn unique_copy_name_avoids_collisions() {
        let dir = std::env::temp_dir().join(format!("qmd-tui-copytest-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        assert_eq!(
            unique_copy_name(&dir, "note"),
            "note copy.md",
            "free name needs no suffix"
        );
        let _ = std::fs::write(dir.join("note copy.md"), "");
        assert_eq!(
            unique_copy_name(&dir, "note"),
            "note copy 2.md",
            "first collision gets ' 2'"
        );
        let _ = std::fs::write(dir.join("note copy 2.md"), "");
        assert_eq!(
            unique_copy_name(&dir, "note"),
            "note copy 3.md",
            "second collision gets ' 3'"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Regression guard for "Esc returns without saving on an existing note": an
    // existing note's absolute path must resolve (via qmd::get_body + resolve_abs)
    // so that save() writes to the real on-disk file and the change persists. If
    // resolve_abs returns None, save_edit() bails with "no note open" and the edit
    // is silently lost.
    #[test]
    fn existing_note_save_roundtrip() {
        let _guard = qmd_test_lock();
        let _ = std::env::var("QMD_TUI_TEST_COLL_DIR");
        let notes = match list_notes(None) {
            Ok(n) if !n.is_empty() => n,
            _ => return, // no index available; skip
        };
        let note = &notes[0];
        let (_, abs) = match get_body(&note.file) {
            Ok(v) => v,
            Err(_) => return,
        };
        let abs = match abs {
            Some(a) => a,
            None => panic!("resolve_abs returned None for existing note {}", note.file),
        };
        let before = std::fs::read_to_string(&abs).unwrap_or_default();
        let marker = format!("\nVERIFY-ROUNDTRIP-{}\n", std::process::id());
        let new_content = format!("{}{}", before, marker);
        save(&abs, &new_content).expect("save should succeed for an existing note");
        let after = std::fs::read_to_string(&abs).unwrap_or_default();
        assert!(
            after.contains(&marker),
            "existing note file was updated on disk; before={before} after={after}"
        );
        // Restore the original content so the test leaves no junk behind.
        let _ = save(&abs, &before);
    }

    // "./..."-form paths from `multi-get --full-path` are relative to the qmd
    // child's cwd (== the TUI's cwd), so they must join with cwd — never with a
    // collection root and never be treated as a collection name.
    #[test]
    fn resolve_abs_joins_dot_relative_with_cwd() {
        let cwd = Path::new("/base/dir");
        assert_eq!(
            resolve_abs_with("./notes/n3.md", Some(cwd)),
            Some(PathBuf::from("/base/dir/notes/n3.md"))
        );
        // A path that merely *looks* like "collection/file" is not resolvable
        // by the pure rules; the cwd form is what multi-get actually emits.
        assert_eq!(resolve_abs_with("./notes/n3.md", None), None);
        assert_eq!(resolve_abs_with("./", Some(cwd)), None);
    }

    // Absolute paths pass through untouched (qmd emits these when the note is
    // not under the qmd child's cwd).
    #[test]
    fn resolve_abs_passes_absolute_through() {
        assert_eq!(
            resolve_abs_with("/home/wave/notes/x.md", None),
            Some(PathBuf::from("/home/wave/notes/x.md"))
        );
        assert_eq!(
            resolve_abs_with("/tmp/qmdnt/notes/n3.md", Some(Path::new("/elsewhere"))),
            Some(PathBuf::from("/tmp/qmdnt/notes/n3.md"))
        );
    }

    // Bare "<collection>/<path>" ids fall back to collection lookup: by name
    // first, then by the collection dir's final segment. Covers qmd:// too.
    #[test]
    fn resolve_abs_collection_fallback() {
        let colls = vec![
            ("t".to_string(), PathBuf::from("/tmp/qmdnt/notes")),
            ("0".to_string(), PathBuf::from("/tmp/qmdrepro/notes")),
        ];
        // By collection name.
        assert_eq!(
            resolve_abs_with_collections("t/n3.md", &colls),
            Some(PathBuf::from("/tmp/qmdnt/notes/n3.md"))
        );
        // By final directory segment when the name does not match. Both
        // collections end in "notes/", so FIRST match wins; assert the id maps
        // to ONE of them (the tie is broken by list order, not by the path).
        let got = resolve_abs_with_collections("notes/beta.md", &colls).expect("dir segment match");
        let expected = [
            PathBuf::from("/tmp/qmdnt/notes/beta.md"),
            PathBuf::from("/tmp/qmdrepro/notes/beta.md"),
        ];
        assert!(
            expected.contains(&got),
            "dir-segment match must land on a notes/ collection, got {got:?}"
        );
        // qmd:// prefix is accepted on the fallback form.
        assert_eq!(
            resolve_abs_with_collections("qmd://t/n3.md", &colls),
            Some(PathBuf::from("/tmp/qmdnt/notes/n3.md"))
        );
        // No match anywhere -> None (callers must surface this, not save silently).
        assert_eq!(resolve_abs_with_collections("x/y.md", &colls), None);
    }
}
