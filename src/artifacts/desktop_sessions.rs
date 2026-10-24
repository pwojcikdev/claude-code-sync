//! The Claude desktop app's session list — what its Code tab shows in the
//! sidebar.
//!
//! The app keeps one JSON file per session under
//! `<app data>/claude-code-sessions/<org>/<account>/local_<id>.json`, and a
//! `deleted_<transcript id>` tombstone for every transcript whose session was
//! deleted, so it never re-adopts that transcript from `~/.claude/projects`.
//! Syncing transcripts alone leaves the other machine's sidebar empty: the
//! app lists what its index holds, and only then finds the transcript by
//! searching every project directory for `<transcript id>.jsonl`.
//!
//! The app rewrites an entry on every focus and every turn, so two machines
//! that both opened a session always hold two versions of its file. An
//! overwrite or a hold-back would lose one side on every sync; instead both
//! directions merge field by field ([`merge_index`]), three-way against the
//! version this machine last synced when it still has it (see
//! [`store_base`]), and otherwise letting the side with the later activity
//! win. Tombstones only ever accumulate, and an entry whose transcript is
//! tombstoned anywhere is removed everywhere.

use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use super::bases;
use super::registry::ARTIFACTS_SUBDIR;
use crate::filter::FilterConfig;
use crate::scm::{ConflictChoice, ConflictedFile};

/// Subdirectory of the sync repository's `artifacts/` root.
pub const REPO_SUBDIR: &str = "desktop-sessions";

/// The repository's root for this category.
pub fn repo_category_root(repo_root: &Path) -> PathBuf {
    repo_root.join(ARTIFACTS_SUBDIR).join(REPO_SUBDIR)
}

/// A session entry: `local_<id>.json`.
const INDEX_PREFIX: &str = "local_";
const INDEX_SUFFIX: &str = ".json";
/// A tombstone: `deleted_<transcript id>`, holding the deletion time in
/// epoch milliseconds.
const TOMBSTONE_PREFIX: &str = "deleted_";

/// The app's data directory name under the platform config directory
/// (`~/Library/Application Support`, `~/.config`, `%APPDATA%`).
const APP_DIR: &str = "Claude";
const SESSIONS_DIR: &str = "claude-code-sessions";

/// Where this machine keeps the last-synced bytes of each entry, by content
/// hash, for the three-way merge. Per machine, never synced.
const BASES_DIR: &str = ".claude-code-sync-desktop-bases";

/// The directory the desktop app keeps its session list in on this platform.
pub fn default_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join(APP_DIR).join(SESSIONS_DIR))
}

/// The session-list directory this configuration syncs, as
/// `FilterConfig::load` resolved it (the configured override, else the
/// platform default). `None` for a config built in code: the category then
/// touches nothing — the engine never reaches outside the paths it was given.
pub fn dir(filter: &FilterConfig) -> Option<PathBuf> {
    filter.desktop_sessions_root.clone()
}

/// Whether a file name is a session entry.
pub fn is_index_file(name: &str) -> bool {
    name.len() > INDEX_PREFIX.len() + INDEX_SUFFIX.len()
        && name.starts_with(INDEX_PREFIX)
        && name.ends_with(INDEX_SUFFIX)
}

/// Whether a file name is a tombstone.
pub fn is_tombstone(name: &str) -> bool {
    name.len() > TOMBSTONE_PREFIX.len() && name.starts_with(TOMBSTONE_PREFIX)
}

/// The transcript id a tombstone names.
fn tombstoned_id(name: &str) -> Option<&str> {
    is_tombstone(name).then(|| &name[TOMBSTONE_PREFIX.len()..])
}

/// Whether a path is a session entry (by its file name).
pub fn is_index_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_index_file)
}

/// Whether a category-relative path is one this category carries: exactly
/// `<org>/<account>/<entry or tombstone>`. Everything else the app keeps
/// beside them (`backlog/`, `archived-sessions.idx`, `scheduled-tasks.json`,
/// in-flight `.json.tmp` writes) stays on the machine — enforced in both
/// directions, so a sync repository cannot plant other files there either.
pub fn is_syncable_rel(rel: &Path) -> bool {
    let mut names = Vec::new();
    for component in rel.components() {
        let Component::Normal(name) = component else {
            return false;
        };
        let Some(name) = name.to_str() else {
            return false;
        };
        names.push(name);
    }
    names.len() == 3 && (is_index_file(names[2]) || is_tombstone(names[2]))
}

/// The transcript id an entry points at, when it parses.
pub fn cli_session_id(bytes: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(bytes)
        .ok()?
        .get("cliSessionId")?
        .as_str()
        .map(str::to_string)
}

/// Every transcript id tombstoned under `root` (the app's directory or the
/// repository's category root): `<org>/<account>/deleted_*`.
pub fn tombstoned_ids(root: &Path) -> HashSet<String> {
    let mut ids = HashSet::new();
    let subdirs = |dir: &Path| -> Vec<PathBuf> {
        fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default()
    };
    for org in subdirs(root) {
        for account in subdirs(&org) {
            let Ok(entries) = fs::read_dir(&account) else {
                continue;
            };
            for entry in entries.filter_map(|e| e.ok()) {
                let name = entry.file_name();
                if let Some(id) = name.to_str().and_then(tombstoned_id) {
                    ids.insert(id.to_string());
                }
            }
        }
    }
    ids
}

/// Every entry under `root` (the app's directory or the repository's
/// category root) whose transcript is one of `ids`, with that id — what a
/// purge of those transcripts retires.
pub fn entries_for_transcripts(root: &Path, ids: &HashSet<String>) -> Vec<(PathBuf, String)> {
    if ids.is_empty() || !root.is_dir() {
        return Vec::new();
    }
    walkdir::WalkDir::new(root)
        .min_depth(3)
        .max_depth(3)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| {
            let path = entry.path().to_path_buf();
            let rel = path.strip_prefix(root).ok()?;
            if !is_syncable_rel(rel) || !is_index_path(rel) {
                return None;
            }
            let id = cli_session_id(&fs::read(&path).ok()?)?;
            ids.contains(&id).then_some((path, id))
        })
        .collect()
}

/// Where the tombstone for `transcript_id` goes: beside the entry, in its
/// account directory, as the app writes it.
pub fn tombstone_beside(entry: &Path, transcript_id: &str) -> PathBuf {
    entry
        .parent()
        .unwrap_or(Path::new(""))
        .join(format!("{TOMBSTONE_PREFIX}{transcript_id}"))
}

// ---------------------------------------------------------------------------
// Merging
// ---------------------------------------------------------------------------

/// Fields that only ever move forward: the later value wins outright.
const LATEST_WINS: &[&str] = &[
    "lastActivityAt",
    "lastFocusedAt",
    "latestUserFrameAt",
    "indexedAt",
    "errorAt",
    "completedTurns",
    "titleTurn",
];
/// Fields that name a beginning: the earlier value wins.
const EARLIEST_WINS: &[&str] = &["createdAt"];
/// Lists that grow as a session is used on either machine: merged as sets,
/// keeping what either side added and dropping what either side removed.
const UNION_LISTS: &[&str] = &[
    "priorCliSessionIds",
    "bridgeSessionIds",
    "writtenBranches",
    "prs",
    "alwaysAllowedReasons",
    "sessionPermissionUpdates",
    "rewindEdges",
    "resolvedBackgroundTaskSuggestions",
    "backgroundTaskSuggestions",
];
/// Caches the app rebuilds per machine (connector tool schemas, the prompt
/// append it last sent, the tool surface it last saw): this machine's copy
/// is kept, the other machine's is only taken when there is none here.
const MACHINE_LOCAL: &[&str] = &[
    "remoteMcpServersConfig",
    "promptAppendSnapshot",
    "toolSurfaceSnapshot",
];

/// Merge the two versions of one file in this category. `base` is the
/// version both sides started from, when this machine still holds it.
pub fn merge_bytes(name: &str, local: &[u8], remote: &[u8], base: Option<&[u8]>) -> Vec<u8> {
    if is_tombstone(name) {
        merge_tombstone(local, remote)
    } else {
        merge_index(local, remote, base)
    }
}

/// Two tombstones for one transcript: the later deletion time.
pub fn merge_tombstone(local: &[u8], remote: &[u8]) -> Vec<u8> {
    match (deletion_time(local), deletion_time(remote)) {
        (Some(ours), Some(theirs)) if theirs > ours => remote.to_vec(),
        (None, Some(_)) => remote.to_vec(),
        _ => local.to_vec(),
    }
}

fn deletion_time(bytes: &[u8]) -> Option<i64> {
    std::str::from_utf8(bytes).ok()?.trim().parse().ok()
}

/// Merge two versions of a session entry field by field (see the module
/// doc). A side that is not a JSON object cannot be merged: the other side
/// is taken, and this machine's bytes when neither parses.
pub fn merge_index(local: &[u8], remote: &[u8], base: Option<&[u8]>) -> Vec<u8> {
    let (Some(ours), Some(theirs)) = (object(local), object(remote)) else {
        return match object(remote) {
            Some(_) if object(local).is_none() => remote.to_vec(),
            _ => local.to_vec(),
        };
    };
    let base = base.and_then(object);
    let remote_later = activity(&theirs) > activity(&ours);
    let merged = merge_objects(&ours, &theirs, base.as_ref(), remote_later, true);
    serde_json::to_vec(&Value::Object(merged)).unwrap_or_else(|_| local.to_vec())
}

fn object(bytes: &[u8]) -> Option<Map<String, Value>> {
    match serde_json::from_slice::<Value>(bytes).ok()? {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

/// When the session was last used on a side, as the app records it: the
/// last turn first, the last time it was opened as the tie-breaker. Equal
/// activity reads as "this machine is later".
fn activity(map: &Map<String, Value>) -> (i64, i64) {
    let at = |key: &str| map.get(key).and_then(Value::as_i64).unwrap_or(i64::MIN);
    (at("lastActivityAt"), at("lastFocusedAt"))
}

fn merge_objects(
    ours: &Map<String, Value>,
    theirs: &Map<String, Value>,
    base: Option<&Map<String, Value>>,
    remote_later: bool,
    top_level: bool,
) -> Map<String, Value> {
    let mut merged = Map::new();
    let keys = ours
        .keys()
        .chain(theirs.keys().filter(|key| !ours.contains_key(*key)));
    for key in keys {
        let field = Field {
            key,
            ours: ours.get(key),
            theirs: theirs.get(key),
            base: base.map(|base| base.get(key)),
            remote_later,
            top_level,
        };
        if let Some(value) = field.merge() {
            merged.insert(key.clone(), value);
        }
    }
    merged
}

/// One field of an entry, as it stands on each side. `base` is `None` when
/// no common version is known, and `Some(None)` when the field was absent
/// from it — the distinction is what tells "added here" from "deleted
/// there".
struct Field<'a> {
    key: &'a str,
    ours: Option<&'a Value>,
    theirs: Option<&'a Value>,
    base: Option<Option<&'a Value>>,
    remote_later: bool,
    top_level: bool,
}

impl Field<'_> {
    fn merge(&self) -> Option<Value> {
        if self.ours == self.theirs {
            return self.ours.cloned();
        }
        if self.top_level {
            if MACHINE_LOCAL.contains(&self.key) {
                return self.ours.or(self.theirs).cloned();
            }
            if LATEST_WINS.contains(&self.key) {
                if let Some(value) = self.extreme(|ours, theirs| theirs > ours) {
                    return Some(value);
                }
            }
            if EARLIEST_WINS.contains(&self.key) {
                if let Some(value) = self.extreme(|ours, theirs| theirs < ours) {
                    return Some(value);
                }
            }
            if UNION_LISTS.contains(&self.key) {
                if let (Some(Value::Array(ours)), Some(Value::Array(theirs))) =
                    (self.ours, self.theirs)
                {
                    let base = self.base.map(|base| base.and_then(Value::as_array));
                    return Some(Value::Array(merge_lists(ours, theirs, base)));
                }
            }
        }
        if let (Some(Value::Object(ours)), Some(Value::Object(theirs))) = (self.ours, self.theirs) {
            let base = self.base.and_then(|base| base.and_then(Value::as_object));
            return Some(Value::Object(merge_objects(
                ours,
                theirs,
                base,
                self.remote_later,
                false,
            )));
        }
        self.three_way().cloned()
    }

    /// The numeric value `prefer_theirs` picks, when both sides hold one.
    fn extreme(&self, prefer_theirs: fn(f64, f64) -> bool) -> Option<Value> {
        let ours = self.ours?.as_f64()?;
        let theirs = self.theirs?.as_f64()?;
        Some(if prefer_theirs(ours, theirs) {
            self.theirs?.clone()
        } else {
            self.ours?.clone()
        })
    }

    /// The side that changed the field since the base, when only one did;
    /// otherwise the side with the later activity.
    fn three_way(&self) -> Option<&Value> {
        if let Some(base) = self.base {
            if self.ours == base {
                return self.theirs;
            }
            if self.theirs == base {
                return self.ours;
            }
        }
        if self.remote_later {
            self.theirs
        } else {
            self.ours
        }
    }
}

/// Set-merge two lists: this machine's order first, then what the other
/// side added. With a base, an item either side removed stays removed.
fn merge_lists(ours: &[Value], theirs: &[Value], base: Option<Option<&Vec<Value>>>) -> Vec<Value> {
    let base: &[Value] = match base {
        Some(Some(base)) => base,
        _ => &[],
    };
    let removed_here: Vec<&Value> = base.iter().filter(|item| !ours.contains(item)).collect();
    let removed_there: Vec<&Value> = base.iter().filter(|item| !theirs.contains(item)).collect();
    let mut merged: Vec<Value> = ours
        .iter()
        .filter(|item| !removed_there.contains(item))
        .cloned()
        .collect();
    for item in theirs {
        if !merged.contains(item) && !removed_here.contains(&item) {
            merged.push(item.clone());
        }
    }
    merged
}

/// Settle a file both machines committed to the sync repository, or `None`
/// when the path is not this category's. Two entries merge; a tombstone is
/// never dropped; and an entry one side deleted stays deleted — the deletion
/// is a decision the tombstones carry, not a lost edit.
pub fn settle_repo_conflict(file: &ConflictedFile) -> Option<ConflictChoice> {
    let category_root = Path::new(ARTIFACTS_SUBDIR).join(REPO_SUBDIR);
    let rel = Path::new(&file.path).strip_prefix(&category_root).ok()?;
    if !is_syncable_rel(rel) {
        return None;
    }
    let name = rel.file_name()?.to_str()?;
    let tombstone = is_tombstone(name);
    Some(match (&file.local, &file.remote) {
        (Some(local), Some(remote)) => {
            ConflictChoice::WriteMerged(merge_bytes(name, local, remote, file.base.as_deref()))
        }
        (None, Some(_)) if tombstone => ConflictChoice::TakeRemote,
        (None, Some(_)) => ConflictChoice::KeepLocal,
        (Some(_), None) if tombstone => ConflictChoice::KeepLocal,
        (Some(_), None) => ConflictChoice::TakeRemote,
        (None, None) => return None,
    })
}

// ---------------------------------------------------------------------------
// The last-synced bytes, kept for the three-way merge
// ---------------------------------------------------------------------------

/// Where the last-synced bytes live, one file per content hash.
pub fn bases_dir(claude_dir: &Path) -> PathBuf {
    claude_dir.join(BASES_DIR)
}

/// Keep `bytes` as a merge base, under the same hash the base record files
/// them by. Idempotent.
pub fn store_base(claude_dir: &Path, bytes: &[u8]) -> Result<()> {
    let dir = bases_dir(claude_dir);
    let path = dir.join(bases::hash_bytes(bytes));
    if path.is_file() {
        return Ok(());
    }
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = tempfile::NamedTempFile::new_in(&dir)?;
    fs::write(tmp.path(), bytes)?;
    tmp.persist(&path)
        .with_context(|| format!("cannot keep merge base {}", path.display()))?;
    Ok(())
}

/// The bytes a recorded base hash names, when this machine still holds them
/// and they are a version both sides once had.
pub fn load_base(claude_dir: &Path, hash: &str) -> Option<Vec<u8>> {
    if !bases::is_common_ancestor(hash) || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let bytes = fs::read(bases_dir(claude_dir).join(hash)).ok()?;
    (bases::hash_bytes(&bytes) == hash).then_some(bytes)
}

/// Drop every kept base no record refers to any more. A base younger than
/// an hour is left alone: a sync against another repository may have kept
/// it moments ago and not saved its record yet.
pub fn prune_bases(claude_dir: &Path, keep: &HashSet<String>) {
    let Ok(entries) = fs::read_dir(bases_dir(claude_dir)) else {
        return;
    };
    let recent = std::time::Duration::from_secs(60 * 60);
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if keep.contains(name) {
            continue;
        }
        let is_recent = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age < recent);
        if is_recent {
            continue;
        }
        if let Err(e) = fs::remove_file(entry.path()) {
            log::debug!("could not prune merge base {}: {e}", entry.path().display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(fields: Value) -> Vec<u8> {
        serde_json::to_vec(&fields).unwrap()
    }

    fn parse(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).unwrap()
    }

    #[test]
    fn recognizes_entries_and_tombstones_by_name() {
        assert!(is_index_file("local_8b980d55.json"));
        assert!(!is_index_file("local_8b980d55.json.tmp"));
        assert!(!is_index_file("local_"));
        assert!(!is_index_file("archived-sessions.idx"));
        assert!(is_tombstone("deleted_2def1fa6"));
        assert!(!is_tombstone("deleted_"));
        assert_eq!(tombstoned_id("deleted_abc"), Some("abc"));
    }

    #[test]
    fn only_entries_and_tombstones_two_levels_down_are_syncable() {
        assert!(is_syncable_rel(Path::new("org/account/local_a.json")));
        assert!(is_syncable_rel(Path::new("org/account/deleted_a")));
        assert!(!is_syncable_rel(Path::new(
            "org/account/backlog/tasks.json"
        )));
        assert!(!is_syncable_rel(Path::new(
            "org/account/archived-sessions.idx"
        )));
        assert!(!is_syncable_rel(Path::new(
            "org/account/scheduled-tasks.json"
        )));
        assert!(!is_syncable_rel(Path::new("org/account/local_a.json.tmp")));
        assert!(!is_syncable_rel(Path::new("org/local_a.json")));
        assert!(!is_syncable_rel(Path::new("org/../account/local_a.json")));
        assert!(!is_syncable_rel(Path::new("/org/account/local_a.json")));
    }

    #[test]
    fn tombstoned_ids_walks_org_and_account_directories() {
        let root = tempfile::tempdir().unwrap();
        let account = root.path().join("org").join("account");
        fs::create_dir_all(&account).unwrap();
        fs::write(account.join("deleted_one"), "1785953402585").unwrap();
        fs::write(account.join("deleted_two"), "1785953402586").unwrap();
        fs::write(account.join("local_x.json"), "{}").unwrap();
        fs::write(root.path().join("deleted_too_high"), "1").unwrap();

        let ids = tombstoned_ids(root.path());
        assert_eq!(ids, HashSet::from(["one".to_string(), "two".to_string()]));
        assert!(tombstoned_ids(Path::new("/nonexistent")).is_empty());
    }

    #[test]
    fn entries_for_transcripts_finds_matching_entries_two_levels_down() {
        let root = tempfile::tempdir().unwrap();
        let account = root.path().join("org").join("account");
        fs::create_dir_all(&account).unwrap();
        let old = account.join("local_old.json");
        fs::write(&old, r#"{"cliSessionId":"cli-old"}"#).unwrap();
        fs::write(
            account.join("local_recent.json"),
            r#"{"cliSessionId":"cli-recent"}"#,
        )
        .unwrap();
        fs::write(account.join("local_broken.json"), "{ not json").unwrap();
        fs::write(
            account.join("local_old.json.tmp"),
            r#"{"cliSessionId":"cli-old"}"#,
        )
        .unwrap();
        fs::write(
            root.path().join("local_shallow.json"),
            r#"{"cliSessionId":"cli-old"}"#,
        )
        .unwrap();

        let ids = HashSet::from(["cli-old".to_string(), "cli-missing".to_string()]);
        assert_eq!(
            entries_for_transcripts(root.path(), &ids),
            vec![(old.clone(), "cli-old".to_string())]
        );
        assert!(entries_for_transcripts(root.path(), &HashSet::new()).is_empty());
        assert!(entries_for_transcripts(Path::new("/nonexistent"), &ids).is_empty());
        assert_eq!(
            tombstone_beside(&old, "cli-old"),
            account.join("deleted_cli-old")
        );
        assert_eq!(
            repo_category_root(Path::new("/repo")),
            Path::new("/repo/artifacts/desktop-sessions")
        );
    }

    #[test]
    fn identical_sides_merge_to_themselves() {
        let a = entry(json!({"sessionId": "local_a", "title": "t", "lastActivityAt": 5}));
        assert_eq!(parse(&merge_index(&a, &a, None)), parse(&a));
        assert_eq!(parse(&merge_index(&a, &a, Some(&a))), parse(&a));
    }

    #[test]
    fn timestamps_and_counters_take_the_later_value_whoever_is_later() {
        let ours = entry(json!({
            "lastActivityAt": 100, "lastFocusedAt": 900, "completedTurns": 3, "createdAt": 10
        }));
        let theirs = entry(json!({
            "lastActivityAt": 200, "lastFocusedAt": 300, "completedTurns": 2, "createdAt": 9
        }));
        let merged = parse(&merge_index(&ours, &theirs, None));
        assert_eq!(merged["lastActivityAt"], 200);
        assert_eq!(merged["lastFocusedAt"], 900);
        assert_eq!(merged["completedTurns"], 3);
        assert_eq!(merged["createdAt"], 9);
        // The same from the other side.
        let merged = parse(&merge_index(&theirs, &ours, None));
        assert_eq!(merged["lastActivityAt"], 200);
        assert_eq!(merged["lastFocusedAt"], 900);
        assert_eq!(merged["completedTurns"], 3);
        assert_eq!(merged["createdAt"], 9);
    }

    #[test]
    fn three_way_keeps_each_sides_own_change() {
        // A renamed the session while B archived it: both survive.
        let base = entry(json!({"title": "old", "isArchived": false, "lastActivityAt": 1}));
        let renamed = entry(json!({"title": "new", "isArchived": false, "lastActivityAt": 1}));
        let archived = entry(json!({"title": "old", "isArchived": true, "lastActivityAt": 1}));

        for (ours, theirs) in [(&renamed, &archived), (&archived, &renamed)] {
            let merged = parse(&merge_index(ours, theirs, Some(&base)));
            assert_eq!(merged["title"], "new");
            assert_eq!(merged["isArchived"], true);
        }
    }

    #[test]
    fn three_way_honors_a_field_one_side_removed_or_added() {
        let base = entry(json!({"planPath": "/p", "lastActivityAt": 1}));
        let removed = entry(json!({"lastActivityAt": 1}));
        let added_other = entry(json!({"planPath": "/p", "branch": "main", "lastActivityAt": 1}));

        let merged = parse(&merge_index(&removed, &added_other, Some(&base)));
        assert!(
            merged.get("planPath").is_none(),
            "removed here stays removed"
        );
        assert_eq!(merged["branch"], "main", "added there is taken");

        let merged = parse(&merge_index(&added_other, &removed, Some(&base)));
        assert!(merged.get("planPath").is_none());
        assert_eq!(merged["branch"], "main");
    }

    #[test]
    fn without_a_base_the_side_with_later_activity_wins_conflicts() {
        let ours = entry(json!({"title": "mine", "lastActivityAt": 100, "lastFocusedAt": 500}));
        let theirs = entry(json!({"title": "theirs", "lastActivityAt": 200, "lastFocusedAt": 100}));
        assert_eq!(parse(&merge_index(&ours, &theirs, None))["title"], "theirs");
        assert_eq!(parse(&merge_index(&theirs, &ours, None))["title"], "theirs");

        // Equal activity: the later focus decides; equal again: this side.
        let ours = entry(json!({"title": "mine", "lastActivityAt": 100, "lastFocusedAt": 500}));
        let theirs = entry(json!({"title": "theirs", "lastActivityAt": 100, "lastFocusedAt": 100}));
        assert_eq!(parse(&merge_index(&ours, &theirs, None))["title"], "mine");
        let tie = entry(json!({"title": "theirs", "lastActivityAt": 100, "lastFocusedAt": 500}));
        assert_eq!(parse(&merge_index(&ours, &tie, None))["title"], "mine");
    }

    #[test]
    fn both_changed_the_same_field_falls_back_to_later_activity() {
        let base = entry(json!({"title": "old", "lastActivityAt": 1}));
        let ours = entry(json!({"title": "mine", "lastActivityAt": 5}));
        let theirs = entry(json!({"title": "theirs", "lastActivityAt": 9}));
        assert_eq!(
            parse(&merge_index(&ours, &theirs, Some(&base)))["title"],
            "theirs"
        );
    }

    #[test]
    fn id_lists_union_and_respect_removals_against_the_base() {
        let base = entry(json!({"priorCliSessionIds": ["a", "b"], "lastActivityAt": 1}));
        let ours = entry(json!({"priorCliSessionIds": ["a", "b", "c"], "lastActivityAt": 1}));
        let theirs = entry(json!({"priorCliSessionIds": ["b", "d"], "lastActivityAt": 1}));

        let merged = parse(&merge_index(&ours, &theirs, Some(&base)));
        assert_eq!(merged["priorCliSessionIds"], json!(["b", "c", "d"]));

        // No base: nothing can be told apart from an addition.
        let merged = parse(&merge_index(&ours, &theirs, None));
        assert_eq!(merged["priorCliSessionIds"], json!(["a", "b", "c", "d"]));
    }

    #[test]
    fn machine_local_caches_stay_this_machines() {
        let ours = entry(json!({
            "remoteMcpServersConfig": [{"uuid": "mine"}],
            "lastActivityAt": 1
        }));
        let theirs = entry(json!({
            "remoteMcpServersConfig": [{"uuid": "theirs"}],
            "promptAppendSnapshot": {"append": "x"},
            "lastActivityAt": 9
        }));
        let merged = parse(&merge_index(&ours, &theirs, None));
        assert_eq!(merged["remoteMcpServersConfig"], json!([{"uuid": "mine"}]));
        assert_eq!(
            merged["promptAppendSnapshot"],
            json!({"append": "x"}),
            "a cache this machine lacks is taken"
        );
    }

    #[test]
    fn nested_maps_merge_per_key() {
        let base = entry(json!({"enabledMcpTools": {"a": true, "b": true}, "lastActivityAt": 1}));
        let ours = entry(json!({
            "enabledMcpTools": {"a": false, "b": true, "local:here": true},
            "lastActivityAt": 1
        }));
        let theirs = entry(json!({
            "enabledMcpTools": {"a": true, "b": false, "local:there": true},
            "lastActivityAt": 2
        }));
        let merged = parse(&merge_index(&ours, &theirs, Some(&base)));
        assert_eq!(
            merged["enabledMcpTools"],
            json!({"a": false, "b": false, "local:here": true, "local:there": true})
        );
    }

    #[test]
    fn a_side_that_is_not_an_object_is_not_merged() {
        let good = entry(json!({"title": "t"}));
        assert_eq!(merge_index(b"garbage", &good, None), good);
        assert_eq!(merge_index(&good, b"garbage", None), good);
        assert_eq!(merge_index(b"garbage", b"[]", None), b"garbage".to_vec());
    }

    #[test]
    fn merging_is_idempotent_and_order_independent_in_content() {
        let base = entry(json!({"title": "old", "isArchived": false, "lastActivityAt": 1}));
        let ours = entry(json!({"title": "new", "isArchived": false, "lastActivityAt": 3}));
        let theirs = entry(json!({"title": "old", "isArchived": true, "lastActivityAt": 2}));
        let once = merge_index(&ours, &theirs, Some(&base));
        let again = merge_index(&once, &theirs, Some(&base));
        assert_eq!(once, again);
        assert_eq!(
            parse(&once),
            parse(&merge_index(&theirs, &ours, Some(&base)))
        );
    }

    #[test]
    fn tombstones_keep_the_later_deletion() {
        assert_eq!(
            merge_tombstone(b"1785953402585", b"1785953402586"),
            b"1785953402586"
        );
        assert_eq!(
            merge_tombstone(b"1785953402586", b"1785953402585"),
            b"1785953402586"
        );
        assert_eq!(merge_tombstone(b"junk", b"5"), b"5");
        assert_eq!(merge_tombstone(b"5", b"junk"), b"5");
        assert_eq!(merge_bytes("deleted_x", b"1", b"2", None), b"2");
    }

    fn conflicted(path: &str, local: Option<&[u8]>, remote: Option<&[u8]>) -> ConflictedFile {
        ConflictedFile {
            path: path.to_string(),
            base: None,
            local: local.map(<[u8]>::to_vec),
            remote: remote.map(<[u8]>::to_vec),
        }
    }

    #[test]
    fn repo_conflicts_merge_entries_and_keep_deletions() {
        let ours = entry(json!({"title": "mine", "lastActivityAt": 1}));
        let theirs = entry(json!({"title": "theirs", "lastActivityAt": 2}));
        let path = "artifacts/desktop-sessions/org/acct/local_a.json";

        match settle_repo_conflict(&conflicted(path, Some(&ours), Some(&theirs))) {
            Some(ConflictChoice::WriteMerged(bytes)) => {
                assert_eq!(parse(&bytes)["title"], "theirs")
            }
            other => panic!("expected a merge, got {other:?}"),
        }
        assert_eq!(
            settle_repo_conflict(&conflicted(path, None, Some(&theirs))),
            Some(ConflictChoice::KeepLocal)
        );
        assert_eq!(
            settle_repo_conflict(&conflicted(path, Some(&ours), None)),
            Some(ConflictChoice::TakeRemote)
        );

        let tombstone = "artifacts/desktop-sessions/org/acct/deleted_a";
        assert_eq!(
            settle_repo_conflict(&conflicted(tombstone, None, Some(b"1"))),
            Some(ConflictChoice::TakeRemote)
        );
        assert_eq!(
            settle_repo_conflict(&conflicted(tombstone, Some(b"1"), None)),
            Some(ConflictChoice::KeepLocal)
        );
        assert_eq!(
            settle_repo_conflict(&conflicted(tombstone, Some(b"1"), Some(b"2"))),
            Some(ConflictChoice::WriteMerged(b"2".to_vec()))
        );

        assert!(settle_repo_conflict(&conflicted(path, None, None)).is_none());
        assert!(settle_repo_conflict(&conflicted(
            "artifacts/settings/settings.json",
            Some(&ours),
            Some(&theirs)
        ))
        .is_none());
        assert!(settle_repo_conflict(&conflicted(
            "artifacts/desktop-sessions/org/acct/backlog/tasks.json",
            Some(&ours),
            Some(&theirs)
        ))
        .is_none());
    }

    #[test]
    fn kept_bases_round_trip_by_hash_and_prune() {
        let claude = tempfile::tempdir().unwrap();
        let bytes = entry(json!({"title": "t"}));
        let hash = bases::hash_bytes(&bytes);

        store_base(claude.path(), &bytes).unwrap();
        store_base(claude.path(), &bytes).unwrap();
        assert_eq!(load_base(claude.path(), &hash), Some(bytes.clone()));
        assert_eq!(load_base(claude.path(), "0000"), None);
        assert_eq!(load_base(claude.path(), &bases::held_base(&bytes)), None);
        assert_eq!(load_base(claude.path(), bases::PROTECTION_SENTINEL), None);
        assert_eq!(load_base(claude.path(), "../../etc/passwd"), None);

        // Corrupted on disk: not served.
        fs::write(bases_dir(claude.path()).join(&hash), b"x").unwrap();
        assert_eq!(load_base(claude.path(), &hash), None);

        // Pruning keeps what is referenced, and anything fresh.
        let old = bases_dir(claude.path()).join("old");
        fs::write(&old, b"old").unwrap();
        let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        fs::File::open(&old)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        let fresh = bases_dir(claude.path()).join("fresh");
        fs::write(&fresh, b"fresh").unwrap();
        prune_bases(claude.path(), &HashSet::new());
        assert!(!old.exists(), "an unreferenced old base goes");
        assert!(fresh.exists(), "a fresh one is left for its record to land");
        prune_bases(Path::new("/nonexistent"), &HashSet::new());
    }
}
