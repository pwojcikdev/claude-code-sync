//! Integration tests for transcript retention: what ages out, what never does,
//! and what goes with a transcript when it does.

use std::fs;
use std::path::{Path, PathBuf};

use claude_code_sync::purge::{self, MINIMUM_RETENTION_DAYS};
use tempfile::TempDir;

/// A transcript whose last message is `age_days` old.
fn transcript(projects_dir: &Path, project: &str, session: &str, age_days: i64) -> PathBuf {
    let last_message = chrono::Utc::now() - chrono::Duration::days(age_days);
    write_transcript(
        projects_dir,
        project,
        session,
        &format!(
            "{{\"type\":\"user\",\"uuid\":\"u1\",\"sessionId\":\"{session}\",\
             \"cwd\":\"/home/user/app\",\"timestamp\":\"{}\"}}\n",
            last_message.to_rfc3339()
        ),
    )
}

fn write_transcript(projects_dir: &Path, project: &str, session: &str, body: &str) -> PathBuf {
    let dir = projects_dir.join(project);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{session}.jsonl"));
    fs::write(&path, body).unwrap();
    path
}

fn purge_with_window(claude_projects: &Path, repo_projects: &Path, days: u32) -> usize {
    let plan = purge::plan(claude_projects, repo_projects, days).unwrap();
    let planned_bytes = plan.total_bytes();
    let report = purge::apply(&plan).unwrap();
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(
        report.bytes, planned_bytes,
        "what was freed is what the plan said it would free"
    );
    report.sessions
}

#[test]
fn an_aged_out_transcript_goes_from_both_sides_at_once() {
    let claude = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    let local = transcript(claude.path(), "-home-user-app", "old-session", 400);
    let stored = transcript(repo.path(), "-home-user-app", "old-session", 400);

    let purged = purge_with_window(claude.path(), repo.path(), MINIMUM_RETENTION_DAYS);

    assert_eq!(purged, 1, "one session, both copies");
    assert!(!local.exists());
    assert!(!stored.exists());
}

#[test]
fn a_transcript_inside_the_window_is_untouched() {
    let claude = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    let recent = transcript(claude.path(), "-home-user-app", "recent", 179);

    let plan = purge::plan(claude.path(), repo.path(), MINIMUM_RETENTION_DAYS).unwrap();
    purge::apply(&plan).unwrap();

    assert!(plan.is_empty());
    assert!(recent.is_file());
}

#[test]
fn age_comes_from_the_last_message_not_the_files_mtime() {
    let claude = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    // Exactly what a pull produces: an old conversation written to disk today.
    let pulled = transcript(claude.path(), "-home-user-app", "pulled-today", 400);
    let mtime = fs::metadata(&pulled).unwrap().modified().unwrap();
    assert!(
        mtime.elapsed().unwrap().as_secs() < 60,
        "the file itself is brand new"
    );

    let purged = purge_with_window(claude.path(), repo.path(), MINIMUM_RETENTION_DAYS);

    assert_eq!(purged, 1);
    assert!(!pulled.exists());
}

#[test]
fn a_transcript_with_no_timestamp_is_never_purged() {
    let claude = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    let undated = write_transcript(
        claude.path(),
        "-home-user-app",
        "undated",
        "{\"type\":\"user\",\"uuid\":\"u1\",\"sessionId\":\"undated\"}\n",
    );

    let plan = purge::plan(claude.path(), repo.path(), MINIMUM_RETENTION_DAYS).unwrap();
    purge::apply(&plan).unwrap();

    assert_eq!(plan.undated, 1);
    assert!(plan.is_empty());
    assert!(undated.is_file(), "an unknown age is not an old age");
}

#[test]
fn everything_claude_code_keeps_beside_a_transcript_goes_with_it() {
    let claude = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    let old = transcript(claude.path(), "-home-user-app", "old-session", 400);
    let project = old.parent().unwrap().to_path_buf();
    fs::create_dir_all(project.join("old-session/subagents")).unwrap();
    fs::write(project.join("old-session/subagents/sub.jsonl"), "{}\n").unwrap();
    fs::create_dir_all(project.join("old-session/tool-results")).unwrap();
    fs::write(project.join("old-session/tool-results/out.txt"), "output").unwrap();
    fs::write(project.join("old-session.orphaned-123-a.jsonl"), "{}\n").unwrap();
    fs::write(project.join("old-session.jsonl.superseded-123"), "{}\n").unwrap();
    // A different session in the same project must survive untouched.
    let keep = transcript(claude.path(), "-home-user-app", "recent", 10);

    purge_with_window(claude.path(), repo.path(), MINIMUM_RETENTION_DAYS);

    assert!(!old.exists());
    assert!(!project.join("old-session").exists());
    assert!(!project.join("old-session.orphaned-123-a.jsonl").exists());
    assert!(!project.join("old-session.jsonl.superseded-123").exists());
    assert!(keep.is_file());
}

#[test]
fn a_subagent_transcript_never_ages_out_on_its_own() {
    let claude = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    let parent = transcript(claude.path(), "-home-user-app", "parent", 10);
    let session_dir = parent.parent().unwrap().join("parent");
    fs::create_dir_all(session_dir.join("subagents")).unwrap();
    let subagent = session_dir.join("subagents/agent.jsonl");
    let long_ago = chrono::Utc::now() - chrono::Duration::days(400);
    fs::write(
        &subagent,
        format!(
            "{{\"type\":\"user\",\"uuid\":\"u9\",\"sessionId\":\"agent\",\
             \"timestamp\":\"{}\"}}\n",
            long_ago.to_rfc3339()
        ),
    )
    .unwrap();

    let plan = purge::plan(claude.path(), repo.path(), MINIMUM_RETENTION_DAYS).unwrap();
    purge::apply(&plan).unwrap();

    assert!(
        plan.is_empty(),
        "the parent session is well inside the window"
    );
    assert!(parent.is_file());
    assert!(
        subagent.is_file(),
        "a subagent belongs to its session, not to its own age"
    );
}

#[test]
fn a_session_used_recently_elsewhere_is_not_old_here() {
    // The same session, stale on one side and recent on the other: whichever
    // side is fresh keeps both copies.
    for (local_age, repo_age) in [(400, 3), (3, 400)] {
        let claude = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let local = transcript(claude.path(), "-home-user-app", "shared", local_age);
        let remote = transcript(repo.path(), "-home-user-app", "shared", repo_age);

        let plan = purge::plan(claude.path(), repo.path(), MINIMUM_RETENTION_DAYS).unwrap();
        purge::apply(&plan).unwrap();

        assert!(plan.is_empty(), "local {local_age}d, repo {repo_age}d");
        assert!(local.is_file(), "local {local_age}d, repo {repo_age}d");
        assert!(remote.is_file(), "local {local_age}d, repo {repo_age}d");
    }
}

#[test]
fn an_unreadable_copy_keeps_the_whole_session() {
    let claude = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    let old = transcript(claude.path(), "-home-user-app", "half-written", 400);
    let mid_write = write_transcript(repo.path(), "-home-user-app", "half-written", "{\"type\"");

    let plan = purge::plan(claude.path(), repo.path(), MINIMUM_RETENTION_DAYS).unwrap();
    purge::apply(&plan).unwrap();

    assert_eq!(plan.unreadable, 1);
    assert!(plan.is_empty());
    assert!(old.is_file(), "a copy that cannot be read dates nothing");
    assert!(mid_write.is_file());
}

#[test]
fn a_sessions_companion_files_go_with_it() {
    let claude = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    let old = transcript(claude.path(), "-home-user-app", "old-session", 400);
    let project = old.parent().unwrap().to_path_buf();
    let stopoffset = project.join("old-session.jsonl.stopoffset");
    fs::write(&stopoffset, "42").unwrap();

    purge_with_window(claude.path(), repo.path(), MINIMUM_RETENTION_DAYS);

    assert!(!old.exists());
    assert!(!stopoffset.exists());
}

#[test]
fn a_shorter_window_purges_more() {
    let claude = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    transcript(claude.path(), "-home-user-app", "a", 400);
    transcript(claude.path(), "-home-user-app", "b", 200);
    transcript(claude.path(), "-home-user-app", "c", 5);

    let plan = purge::plan(claude.path(), repo.path(), 30).unwrap();

    let purged: Vec<&str> = plan
        .targets
        .iter()
        .map(|target| target.session_id.as_str())
        .collect();
    assert_eq!(purged, vec!["a", "b"], "oldest first, newest kept");
    assert_eq!(plan.oldest_age_days(), 400);
}

// ---------------------------------------------------------------------------
// The desktop app's session entries go with their transcripts
// ---------------------------------------------------------------------------

use claude_code_sync::artifacts::{bases, tracked};

/// A desktop app session entry under `root/org-1/account-1/local_<id>.json`
/// pointing at transcript `cli`.
fn desktop_entry(root: &Path, id: &str, cli: &str) -> PathBuf {
    let account = root.join("org-1").join("account-1");
    fs::create_dir_all(&account).unwrap();
    let path = account.join(format!("local_{id}.json"));
    fs::write(
        &path,
        format!(r#"{{"sessionId":"local_{id}","cliSessionId":"{cli}","title":"t"}}"#),
    )
    .unwrap();
    path
}

fn tombstone(entry: &Path, cli: &str) -> PathBuf {
    entry.parent().unwrap().join(format!("deleted_{cli}"))
}

#[test]
fn desktop_entries_for_purged_transcripts_are_retired_with_tombstones() {
    let claude = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    let desktop = TempDir::new().unwrap();
    let projects = claude.path().join("projects");
    let repo_projects = repo.path().join("projects");
    transcript(&projects, "-home-user-app", "old-session", 400);
    transcript(&repo_projects, "-home-user-app", "old-session", 400);
    transcript(&projects, "-home-user-app", "recent", 10);

    let old_here = desktop_entry(desktop.path(), "a", "old-session");
    let recent_here = desktop_entry(desktop.path(), "b", "recent");
    let category = repo.path().join("artifacts").join("desktop-sessions");
    let old_in_repo = desktop_entry(&category, "a", "old-session");
    let recent_in_repo = desktop_entry(&category, "b", "recent");
    // This machine's records file the repository copy, as a sync would.
    let repo_rel = "artifacts/desktop-sessions/org-1/account-1/local_a.json".to_string();
    bases::save(
        claude.path(),
        repo.path(),
        [(repo_rel.clone(), "hash".to_string())]
            .into_iter()
            .collect(),
    )
    .unwrap();
    tracked::save(
        claude.path(),
        repo.path(),
        [repo_rel.clone()].into_iter().collect(),
    )
    .unwrap();

    let mut plan = purge::plan(&projects, &repo_projects, MINIMUM_RETENTION_DAYS).unwrap();
    purge::retire_desktop_entries(&mut plan, Some(desktop.path()), repo.path());
    assert_eq!(plan.targets.len(), 1);
    assert_eq!(plan.desktop_entries.len(), 2, "{:?}", plan.desktop_entries);
    let repo_entry = plan
        .desktop_entries
        .iter()
        .find(|e| e.path == old_in_repo)
        .expect("the repository copy is planned");
    assert_eq!(repo_entry.repo_rel.as_deref(), Some(repo_rel.as_str()));
    assert_eq!(repo_entry.transcript_id, "old-session");
    // Planning (a dry run) touches nothing.
    assert!(old_here.is_file() && old_in_repo.is_file());
    assert!(!tombstone(&old_here, "old-session").exists());

    let report = purge::apply(&plan).unwrap();
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(report.sessions, 1);
    assert_eq!(report.desktop_entries, 2);
    assert!(!old_here.exists());
    assert!(!old_in_repo.exists());
    assert!(
        recent_here.is_file(),
        "an entry for a kept transcript stays"
    );
    assert!(recent_in_repo.is_file());
    for stone in [
        tombstone(&old_here, "old-session"),
        tombstone(&old_in_repo, "old-session"),
    ] {
        let at: i64 = fs::read_to_string(&stone).unwrap().trim().parse().unwrap();
        assert!(
            at > 1_700_000_000_000,
            "epoch milliseconds, as the app writes"
        );
    }
    assert!(!tombstone(&recent_here, "recent").exists());

    purge::forget_desktop_records(claude.path(), repo.path(), &plan).unwrap();
    assert!(!bases::load(claude.path(), repo.path()).contains_key(&repo_rel));
    assert!(!tracked::load(claude.path(), repo.path()).contains(&repo_rel));
}

#[test]
fn an_existing_tombstone_is_kept_and_a_machine_without_the_app_only_touches_the_repo() {
    let claude = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    let projects = claude.path().join("projects");
    let repo_projects = repo.path().join("projects");
    transcript(&repo_projects, "-home-user-app", "old-session", 400);
    let category = repo.path().join("artifacts").join("desktop-sessions");
    let old_in_repo = desktop_entry(&category, "a", "old-session");
    fs::write(tombstone(&old_in_repo, "old-session"), "1785953402585").unwrap();

    let mut plan = purge::plan(&projects, &repo_projects, MINIMUM_RETENTION_DAYS).unwrap();
    purge::retire_desktop_entries(&mut plan, None, repo.path());
    assert_eq!(plan.desktop_entries.len(), 1);

    let report = purge::apply(&plan).unwrap();
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert!(!old_in_repo.exists());
    assert_eq!(
        fs::read_to_string(tombstone(&old_in_repo, "old-session")).unwrap(),
        "1785953402585",
        "an earlier deletion keeps its time"
    );
    // Nothing to forget: no repository entry was recorded here.
    purge::forget_desktop_records(claude.path(), repo.path(), &plan).unwrap();

    // And with nothing purged, nothing is retired either.
    let mut empty = purge::plan(&projects, &repo_projects, MINIMUM_RETENTION_DAYS).unwrap();
    purge::retire_desktop_entries(&mut empty, None, repo.path());
    assert!(empty.desktop_entries.is_empty());
}

#[test]
fn the_other_machine_drops_its_entry_on_the_next_pull() {
    use claude_code_sync::artifacts::engine::{apply_pull, plan_pull};
    use claude_code_sync::artifacts::registry::ArtifactToggles;
    use claude_code_sync::filter::FilterConfig;

    // Machine A purged: the repository holds the tombstone and no entry.
    let repo = TempDir::new().unwrap();
    let category = repo.path().join("artifacts").join("desktop-sessions");
    let purged = desktop_entry(&category, "a", "old-session");
    fs::remove_file(&purged).unwrap();
    fs::write(tombstone(&purged, "old-session"), "1785953402585").unwrap();
    desktop_entry(&category, "b", "recent");

    // Machine B still lists the session.
    let claude_b = TempDir::new().unwrap();
    let desktop_b = TempDir::new().unwrap();
    let entry_b = desktop_entry(desktop_b.path(), "a", "old-session");
    let filter = FilterConfig {
        sync_artifacts: ArtifactToggles {
            desktop_sessions: true,
            ..Default::default()
        },
        desktop_sessions_root: Some(desktop_b.path().to_path_buf()),
        ..Default::default()
    };

    let plan = plan_pull(claude_b.path(), repo.path(), &filter).unwrap();
    assert_eq!(plan.tombstoned_deletes.len(), 1);
    assert_eq!(plan.creates.len(), 2, "the tombstone and the kept entry");
    apply_pull(&plan, false).unwrap();
    assert!(!entry_b.exists());
    assert_eq!(
        fs::read_to_string(tombstone(&entry_b, "old-session")).unwrap(),
        "1785953402585"
    );
    assert!(entry_b.parent().unwrap().join("local_b.json").is_file());
}
