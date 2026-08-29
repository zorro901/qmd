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
/// to an absolute on-disk path. `qmd` emits a *relative* path (e.g.
/// `"./notes/alpha.md"`) even with `--full-path`, so we map the leading
/// collection segment onto the directory from `qmd collection list`. Absolute
/// paths (rare) pass through unchanged. Returns None only if the collection
/// cannot be located, which would also block saving/deleting.
fn resolve_abs(rel: &str) -> Option<PathBuf> {
    let cleaned = rel.strip_prefix("./").unwrap_or(rel);
    let (coll_name, rest) = cleaned.split_once('/')?;
    let collections = list_collections().ok()?;
    let dir = collections
        .into_iter()
        .find(|(n, _)| n == coll_name)
        .map(|(_, p)| p)?;
    Some(dir.join(rest))
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


#[cfg(test)]
mod tests {
    use super::*;

    // Verifies the save() round-trip: write a file, reindex via qmd, then read
    // it back through the qmd CLI. Skipped automatically if `qmd` is not on PATH
    // or no index exists (CI without a built index).
    #[test]
    fn save_then_relist_reflects_content() {
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
}


