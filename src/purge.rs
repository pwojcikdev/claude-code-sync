//! Retention for conversation transcripts.
//!
//! Transcripts past the retention window are removed from `~/.claude/projects`
//! and the sync repository together: removing them on one side only means the
//! next sync restores them from the other.
//!
//! The window is never shorter than `cleanupPeriodDays`, which is when Claude
//! Code deletes the transcript anyway, nor shorter than six months.
//!
//! The desktop app's session entries that point at a purged transcript go
//! with it, each leaving the tombstone the app itself writes for a deleted
//! session (see `crate::artifacts::desktop_sessions`), so no machine keeps
//! listing the session or re-adopts the transcript.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use rayon::prelude::*;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::artifacts::desktop_sessions;

/// Shortest retention window applied by default: six months.
pub const MINIMUM_RETENTION_DAYS: u32 = 180;

/// Claude Code's documented default for `cleanupPeriodDays`.
pub const CLAUDE_CODE_DEFAULT_RETENTION_DAYS: u32 = 30;

/// One session that has aged out, with every file belonging to it.
#[derive(Debug, Clone)]
pub struct PurgeTarget {
    pub session_id: String,
    /// Timestamp of the last message, which the age is measured from.
    pub last_activity: DateTime<Utc>,
    pub age_days: i64,
    /// Files and directories to remove, across both trees.
    pub paths: Vec<PathBuf>,
    pub bytes: u64,
}

/// One desktop app session entry a purge retires with its transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopEntry {
    /// The entry file, on this machine or in the sync repository.
    pub path: PathBuf,
    /// The purged transcript it points at: what its tombstone names.
    pub transcript_id: String,
    /// The entry's repo-relative path when it is the repository's copy, for
    /// the records that file it under that key.
    pub repo_rel: Option<String>,
}

/// What a purge would remove, computed before anything is deleted.
#[derive(Debug, Default)]
pub struct PurgePlan {
    pub retention_days: u32,
    pub cutoff: Option<DateTime<Utc>>,
    pub targets: Vec<PurgeTarget>,
    /// Desktop app session entries — this machine's and the repository's —
    /// that point at a purged transcript (see [`retire_desktop_entries`]).
    pub desktop_entries: Vec<DesktopEntry>,
    /// Sessions kept because at least one copy has no readable age.
    pub undated: usize,
    /// Transcript files that failed to parse, such as one being written right
    /// now. Their session is kept.
    pub unreadable: usize,
}

impl PurgePlan {
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    pub fn total_bytes(&self) -> u64 {
        self.targets.iter().map(|t| t.bytes).sum()
    }

    /// Age of the oldest session in the plan, in days.
    pub fn oldest_age_days(&self) -> i64 {
        self.targets.iter().map(|t| t.age_days).max().unwrap_or(0)
    }
}

/// What a purge removed.
#[derive(Debug, Default)]
pub struct PurgeReport {
    pub sessions: usize,
    pub files: usize,
    pub bytes: u64,
    /// Desktop app session entries removed, each with a tombstone written.
    pub desktop_entries: usize,
    /// Paths that could not be removed, with the reason.
    pub failures: Vec<String>,
}

/// Retention window in days: the longer of [`MINIMUM_RETENTION_DAYS`] and this
/// machine's `cleanupPeriodDays`, lengthened (never shortened) by a configured
/// override. A purge can run unattended after every sync, so a short window
/// set once, or left in an older config, must not quietly delete recent work.
pub fn retention_days(claude_dir: &Path, configured: Option<u32>) -> u32 {
    let floor = MINIMUM_RETENTION_DAYS.max(claude_code_retention_days(claude_dir));
    match configured {
        Some(days) if days < floor => {
            log::warn!(
                "Purge window of {days} days is below the {floor}-day minimum; using {floor}"
            );
            floor
        }
        Some(days) => days,
        None => floor,
    }
}

/// `cleanupPeriodDays` from the user-level settings files, or
/// [`CLAUDE_CODE_DEFAULT_RETENTION_DAYS`].
///
/// The longest of them wins rather than the one Claude Code's precedence picks:
/// the value only ever holds a deletion back. Project-level settings are not
/// consulted; `--older-than` covers that case.
fn claude_code_retention_days(claude_dir: &Path) -> u32 {
    let configured = ["settings.json", "settings.local.json"]
        .into_iter()
        .filter_map(|name| std::fs::read_to_string(claude_dir.join(name)).ok())
        .filter_map(|text| serde_json::from_str::<Value>(&text).ok())
        .filter_map(|settings| {
            settings
                .get("cleanupPeriodDays")
                .and_then(Value::as_u64)
                .filter(|days| *days > 0)
        })
        .map(|days| u32::try_from(days).unwrap_or(u32::MAX))
        .max();

    configured.unwrap_or(CLAUDE_CODE_DEFAULT_RETENTION_DAYS)
}

/// Every copy of one session found across the trees, before deciding its fate.
#[derive(Default)]
struct SessionCopies {
    last_activity: Option<DateTime<Utc>>,
    /// Set when any copy has no readable age, which keeps the session.
    undatable: bool,
    paths: Vec<PathBuf>,
    bytes: u64,
}

/// Classify what a purge would remove from both project trees, without
/// deleting anything.
///
/// A session is decided as a whole — every copy of it, with its sidecars — and
/// its age is that of the most recent copy.
pub fn plan(
    claude_projects_dir: &Path,
    repo_projects_dir: &Path,
    retention_days: u32,
) -> Result<PurgePlan> {
    let mut plan = PurgePlan {
        retention_days,
        ..Default::default()
    };
    // Checked: a window large enough to overflow has nothing to purge anyway.
    let Some(cutoff) = Utc::now().checked_sub_signed(Duration::days(i64::from(retention_days)))
    else {
        return Ok(plan);
    };
    plan.cutoff = Some(cutoff);

    let mut by_session: std::collections::BTreeMap<String, SessionCopies> =
        std::collections::BTreeMap::new();

    for transcripts_root in [claude_projects_dir, repo_projects_dir] {
        // Summarizing is the whole cost of planning a purge, and every file is
        // independent: read them across the cores, then fold the results in the
        // order the walk found them.
        let summarized: Vec<(PathBuf, Result<crate::parser::ConversationSession>)> =
            transcripts(transcripts_root)
                .into_par_iter()
                .map(|transcript| {
                    let summary = crate::parser::ConversationSession::from_file(&transcript);
                    (transcript, summary)
                })
                .collect();

        for (transcript, summary) in summarized {
            let session = match summary {
                Ok(session) => session,
                Err(error) => {
                    // The session is kept whole: a copy that cannot be read
                    // says nothing about the age of the one that can.
                    log::warn!("Skipping unreadable {}: {error}", transcript.display());
                    plan.unreadable += 1;
                    if let Some(stem) = transcript.file_stem().and_then(|s| s.to_str()) {
                        by_session.entry(stem.to_string()).or_default().undatable = true;
                    }
                    continue;
                }
            };

            let paths: Vec<PathBuf> = std::iter::once(transcript.clone())
                .chain(sidecars(&transcript))
                .collect();
            let bytes: u64 = paths.iter().map(|path| path_size(path)).sum();
            let last_activity = last_activity(&session);

            let copies = by_session.entry(session.session_id.clone()).or_default();
            copies.paths.extend(paths);
            copies.bytes += bytes;
            copies.undatable |= last_activity.is_none();
            copies.last_activity = copies.last_activity.max(last_activity);
        }
    }

    let now = Utc::now();
    for (session_id, copies) in by_session {
        if copies.undatable {
            plan.undated += 1;
            continue;
        }
        let Some(last_activity) = copies.last_activity else {
            continue;
        };
        if last_activity > cutoff {
            continue;
        }
        plan.targets.push(PurgeTarget {
            session_id,
            last_activity,
            age_days: (now - last_activity).num_days(),
            paths: copies.paths,
            bytes: copies.bytes,
        });
    }

    plan.targets.sort_by_key(|target| target.last_activity);
    Ok(plan)
}

/// Add to the plan every desktop app session entry — on this machine under
/// `local_root`, when the app's directory is there, and the repository's —
/// that points at a transcript the plan removes. Applying the plan then
/// removes each and writes the tombstone beside it, the way the app retires
/// a deleted session: the next sync drops the entry from every other
/// machine, and none re-adopts the transcript.
pub fn retire_desktop_entries(plan: &mut PurgePlan, local_root: Option<&Path>, repo_root: &Path) {
    let ids: HashSet<String> = plan
        .targets
        .iter()
        .map(|target| target.session_id.clone())
        .collect();
    let mut entries = Vec::new();
    if let Some(root) = local_root {
        for (path, transcript_id) in desktop_sessions::entries_for_transcripts(root, &ids) {
            entries.push(DesktopEntry {
                path,
                transcript_id,
                repo_rel: None,
            });
        }
    }
    let category_root = desktop_sessions::repo_category_root(repo_root);
    for (path, transcript_id) in desktop_sessions::entries_for_transcripts(&category_root, &ids) {
        let repo_rel = path
            .strip_prefix(repo_root)
            .ok()
            .map(|rel| rel.to_string_lossy().replace('\\', "/"));
        entries.push(DesktopEntry {
            path,
            transcript_id,
            repo_rel,
        });
    }
    plan.desktop_entries = entries;
}

/// Drop the per-machine record entries (last-synced base, tracked path) of
/// the repository entries the plan retires: they describe files that exist
/// nowhere any more, and nothing later prunes them.
pub fn forget_desktop_records(claude_dir: &Path, repo_root: &Path, plan: &PurgePlan) -> Result<()> {
    let removals: Vec<String> = plan
        .desktop_entries
        .iter()
        .filter_map(|entry| entry.repo_rel.clone())
        .collect();
    if removals.is_empty() {
        return Ok(());
    }
    crate::artifacts::bases::save_delta(
        claude_dir,
        repo_root,
        Default::default(),
        removals.clone(),
    )?;
    crate::artifacts::tracked::save_delta(claude_dir, repo_root, Default::default(), removals)
}

/// Delete everything the plan names. A path that is already gone is skipped,
/// not an error.
pub fn apply(plan: &PurgePlan) -> Result<PurgeReport> {
    let mut report = PurgeReport::default();
    let now_ms = Utc::now().timestamp_millis();

    for entry in &plan.desktop_entries {
        match std::fs::remove_file(&entry.path) {
            Ok(()) => report.desktop_entries += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                report
                    .failures
                    .push(format!("{}: {error}", entry.path.display()));
                continue;
            }
        }
        // The tombstone the app would have written: epoch milliseconds, and
        // an existing one (an earlier deletion) is left as it is.
        let tombstone = desktop_sessions::tombstone_beside(&entry.path, &entry.transcript_id);
        if !tombstone.exists() {
            if let Err(error) = std::fs::write(&tombstone, now_ms.to_string()) {
                report
                    .failures
                    .push(format!("{}: {error}", tombstone.display()));
            }
        }
    }

    for target in &plan.targets {
        report.sessions += 1;
        for path in &target.paths {
            let bytes = path_size(path);
            let removed = if path.is_dir() {
                std::fs::remove_dir_all(path)
            } else if path.is_file() {
                std::fs::remove_file(path)
            } else {
                continue;
            };
            match removed {
                Ok(()) => {
                    report.files += 1;
                    report.bytes += bytes;
                }
                Err(error) => report.failures.push(format!("{}: {error}", path.display())),
            }
        }
    }

    Ok(report)
}

/// Open the sync repository, before anything is deleted: a repository that
/// cannot record the removal must stop the purge while the files still exist.
pub fn open_sync_repo(repo_root: &Path) -> Result<Box<dyn crate::scm::Scm>> {
    let repo = crate::scm::open(repo_root)
        .with_context(|| format!("Failed to open repository at {}", repo_root.display()))?;
    repo.has_changes()
        .with_context(|| format!("Cannot read the repository at {}", repo_root.display()))?;
    Ok(repo)
}

/// Refuse to purge while the repository holds uncommitted work.
///
/// A repo-side transcript that is not committed yet — a push that was declined,
/// a path an ignore rule covers — would be deleted with no copy in history,
/// which is the one thing a purge promises not to do.
pub fn ensure_nothing_uncommitted(repo: &dyn crate::scm::Scm) -> Result<()> {
    if repo.has_changes()? {
        bail!(
            "the sync repository has uncommitted changes; run `claude-code-sync push` \
             first, so a purge cannot delete a copy that was never committed"
        );
    }
    Ok(())
}

/// Stage and commit the removals. Returns whether a commit was made.
pub fn commit_removals(repo: &dyn crate::scm::Scm, plan: &PurgePlan) -> Result<bool> {
    repo.stage_all()?;
    if !repo.has_changes()? {
        return Ok(false);
    }
    repo.commit(&format!(
        "Purge {} transcripts older than {} days",
        plan.targets.len(),
        plan.retention_days
    ))?;
    Ok(true)
}

/// Session transcripts of a project tree: `<project>/<session>.jsonl` only.
///
/// Subagent transcripts live deeper, under `<session>/subagents/`, and are
/// removed with their parent session rather than on their own age.
fn transcripts(projects_dir: &Path) -> Vec<PathBuf> {
    if !projects_dir.is_dir() {
        return Vec::new();
    }
    walkdir::WalkDir::new(projects_dir)
        .follow_links(false)
        .min_depth(2)
        .max_depth(2)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.path().to_path_buf())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect()
}

/// Files Claude Code keeps beside a transcript and deletes with it: the
/// session directory (`subagents/`, `tool-results/`) and any set-aside copies.
fn sidecars(transcript: &Path) -> Vec<PathBuf> {
    let Some(directory) = transcript.parent() else {
        return Vec::new();
    };
    let Some(stem) = transcript.file_stem().and_then(|s| s.to_str()) else {
        return Vec::new();
    };

    let mut found = Vec::new();
    let session_directory = directory.join(stem);
    if session_directory.is_dir() {
        found.push(session_directory);
    }

    // Every sibling named after this session: `.orphaned-…`, `.superseded-…`,
    // `.stopoffset`, and whatever Claude Code adds next.
    let companion = format!("{stem}.");
    let transcript_name = transcript.file_name();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return found;
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        if Some(entry.file_name().as_os_str()) == transcript_name {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name.starts_with(&companion) {
            found.push(entry.path());
        }
    }
    found
}

/// Timestamp of the session's last message, in UTC.
fn last_activity(session: &crate::parser::ConversationSession) -> Option<DateTime<Utc>> {
    let stamp = session.latest_timestamp()?;
    DateTime::parse_from_rfc3339(stamp)
        .ok()
        .map(|dated| dated.with_timezone(&Utc))
}

fn path_size(path: &Path) -> u64 {
    if path.is_dir() {
        return walkdir::WalkDir::new(path)
            .follow_links(false)
            .into_iter()
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.metadata().ok())
            .filter(|metadata| metadata.is_file())
            .map(|metadata| metadata.len())
            .sum();
    }
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude_dir_with(settings: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        if let Some(settings) = settings {
            std::fs::write(dir.path().join("settings.json"), settings).unwrap();
        }
        dir
    }

    #[test]
    fn the_default_window_is_six_months() {
        let claude = claude_dir_with(None);
        assert_eq!(retention_days(claude.path(), None), MINIMUM_RETENTION_DAYS);
    }

    #[test]
    fn a_longer_claude_code_window_wins_over_six_months() {
        let claude = claude_dir_with(Some(r#"{"cleanupPeriodDays": 400}"#));
        assert_eq!(retention_days(claude.path(), None), 400);
    }

    #[test]
    fn a_shorter_claude_code_window_does_not_shorten_ours() {
        let claude = claude_dir_with(Some(r#"{"cleanupPeriodDays": 7}"#));
        assert_eq!(retention_days(claude.path(), None), MINIMUM_RETENTION_DAYS);
    }

    #[test]
    fn unreadable_settings_fall_back_to_claude_codes_default() {
        let claude = claude_dir_with(Some("{ not json"));
        assert_eq!(retention_days(claude.path(), None), MINIMUM_RETENTION_DAYS);
        assert_eq!(
            claude_code_retention_days(claude.path()),
            CLAUDE_CODE_DEFAULT_RETENTION_DAYS
        );
    }

    #[test]
    fn the_longest_user_level_window_wins() {
        let claude = claude_dir_with(Some(r#"{"cleanupPeriodDays": 200}"#));
        std::fs::write(
            claude.path().join("settings.local.json"),
            r#"{"cleanupPeriodDays": 400}"#,
        )
        .unwrap();
        assert_eq!(retention_days(claude.path(), None), 400);

        std::fs::write(
            claude.path().join("settings.local.json"),
            r#"{"cleanupPeriodDays": 10}"#,
        )
        .unwrap();
        assert_eq!(
            retention_days(claude.path(), None),
            200,
            "a shorter override never shortens the purge window"
        );
    }

    #[test]
    fn an_absurd_window_leaves_nothing_to_purge_instead_of_panicking() {
        let claude = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let days = retention_days(claude.path(), Some(u32::MAX));

        let plan = plan(claude.path(), repo.path(), days).unwrap();

        assert!(plan.is_empty());
        assert!(plan.cutoff.is_none());
    }

    #[test]
    fn a_configured_window_can_lengthen_the_minimum_but_never_shorten_it() {
        let claude = claude_dir_with(Some(r#"{"cleanupPeriodDays": 400}"#));
        assert_eq!(retention_days(claude.path(), Some(500)), 500);
        assert_eq!(retention_days(claude.path(), Some(30)), 400);
        assert_eq!(retention_days(claude.path(), Some(0)), 400);

        let defaults = claude_dir_with(None);
        assert_eq!(
            retention_days(defaults.path(), Some(1)),
            MINIMUM_RETENTION_DAYS
        );
    }
}
