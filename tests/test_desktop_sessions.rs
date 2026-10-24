//! Integration tests for the `desktop-sessions` category: the Claude desktop
//! app's session list travels between two machines, an entry both machines
//! changed merges field by field, a session deleted in the app disappears
//! everywhere through the app's own tombstones, and nothing else in the
//! app's directory moves. Two temp trees stand in for two machines sharing
//! one sync repository; engine functions take explicit paths, so no
//! environment is touched.

use std::fs;
use std::path::{Path, PathBuf};

use claude_code_sync::artifacts::engine::{
    apply_pull, plan_pull, push_artifacts, ArtifactChangeKind, ArtifactReport, PullPlan,
};
use claude_code_sync::artifacts::registry::{ArtifactToggles, CategoryId};
use claude_code_sync::filter::FilterConfig;
use serde_json::{json, Value};
use tempfile::TempDir;

const ORG: &str = "11111111-aaaa-4bbb-8ccc-000000000001";
const ACCOUNT: &str = "22222222-aaaa-4bbb-8ccc-000000000002";

/// One machine: its `~/.claude` and the desktop app's session directory.
struct Machine {
    _root: TempDir,
    claude: PathBuf,
    desktop: PathBuf,
    filter: FilterConfig,
}

impl Machine {
    fn new() -> Machine {
        let root = TempDir::new().unwrap();
        let claude = root.path().join("claude");
        let desktop = root.path().join("Claude").join("claude-code-sessions");
        fs::create_dir_all(&claude).unwrap();
        fs::create_dir_all(desktop.join(ORG).join(ACCOUNT)).unwrap();
        let filter = FilterConfig {
            sync_artifacts: ArtifactToggles {
                desktop_sessions: true,
                ..Default::default()
            },
            desktop_sessions_root: Some(desktop.clone()),
            ..Default::default()
        };
        Machine {
            _root: root,
            claude,
            desktop,
            filter,
        }
    }

    fn account_dir(&self) -> PathBuf {
        self.desktop.join(ORG).join(ACCOUNT)
    }

    fn entry_path(&self, id: &str) -> PathBuf {
        self.account_dir().join(format!("local_{id}.json"))
    }

    fn write_entry(&self, id: &str, value: &Value) {
        fs::write(self.entry_path(id), serde_json::to_vec(value).unwrap()).unwrap();
    }

    fn read_entry(&self, id: &str) -> Value {
        serde_json::from_slice(&fs::read(self.entry_path(id)).unwrap()).unwrap()
    }

    fn tombstone_path(&self, cli: &str) -> PathBuf {
        self.account_dir().join(format!("deleted_{cli}"))
    }

    fn write_tombstone(&self, cli: &str, at: u64) {
        fs::write(self.tombstone_path(cli), at.to_string()).unwrap();
    }

    fn push(&self, repo: &Path) -> ArtifactReport {
        push_artifacts(
            &self.claude,
            repo,
            &self.filter,
            &std::collections::HashSet::new(),
        )
        .unwrap()
    }

    fn plan(&self, repo: &Path) -> PullPlan {
        plan_pull(&self.claude, repo, &self.filter).unwrap()
    }

    fn pull(&self, repo: &Path) -> ArtifactReport {
        apply_pull(&self.plan(repo), false).unwrap()
    }

    /// What `claude-code-sync sync` does: pull, then push.
    fn sync(&self, repo: &Path) {
        self.pull(repo);
        self.push(repo);
    }
}

fn entry(id: &str, cli: &str, title: &str, activity: u64) -> Value {
    json!({
        "sessionId": format!("local_{id}"),
        "cliSessionId": cli,
        "cwd": "/somewhere/project",
        "originCwd": "/somewhere/project",
        "createdAt": 1000,
        "lastActivityAt": activity,
        "lastFocusedAt": activity,
        "isArchived": false,
        "title": title,
        "model": "claude-fable-5-1",
        "permissionMode": "auto",
        "enabledMcpTools": {"shared:tool": true},
        "remoteMcpServersConfig": [],
        "priorCliSessionIds": [],
    })
}

fn repo_account_dir(repo: &Path) -> PathBuf {
    repo.join("artifacts")
        .join("desktop-sessions")
        .join(ORG)
        .join(ACCOUNT)
}

fn repo_entry(repo: &Path, id: &str) -> PathBuf {
    repo_account_dir(repo).join(format!("local_{id}.json"))
}

fn read_repo_entry(repo: &Path, id: &str) -> Value {
    serde_json::from_slice(&fs::read(repo_entry(repo, id)).unwrap()).unwrap()
}

fn kinds(report: &ArtifactReport, kind: ArtifactChangeKind) -> Vec<String> {
    report
        .changes
        .iter()
        .filter(|c| c.category == CategoryId::DesktopSessions && c.kind == kind)
        .map(|c| c.path.to_string_lossy().replace('\\', "/"))
        .collect()
}

#[test]
fn only_entries_and_tombstones_leave_the_machine() {
    let a = Machine::new();
    let repo = TempDir::new().unwrap();
    a.write_entry("s1", &entry("s1", "cli-1", "First", 100));
    a.write_tombstone("cli-gone", 1_785_953_402_585);
    let account = a.account_dir();
    fs::write(
        account.join("archived-sessions.idx"),
        r#"{"v":1,"archived":[]}"#,
    )
    .unwrap();
    fs::write(account.join("scheduled-tasks.json"), "{}").unwrap();
    fs::create_dir_all(account.join("backlog")).unwrap();
    fs::write(account.join("backlog/tasks.json"), "{}").unwrap();
    fs::write(account.join("local_inflight.json.tmp"), "{}").unwrap();
    fs::write(a.desktop.join(ORG).join("deleted_too-high"), "1").unwrap();
    fs::write(a.desktop.join("local_top.json"), "{}").unwrap();

    let report = a.push(repo.path());

    let mut added = kinds(&report, ArtifactChangeKind::Added);
    added.sort();
    assert_eq!(
        added,
        vec![
            format!("{ORG}/{ACCOUNT}/deleted_cli-gone"),
            format!("{ORG}/{ACCOUNT}/local_s1.json"),
        ]
    );
    let names: Vec<String> = walkdir::WalkDir::new(repo.path().join("artifacts"))
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.contains(&"local_s1.json".to_string()));
    assert!(names.contains(&"deleted_cli-gone".to_string()));
}

#[test]
fn a_repository_cannot_plant_other_files_in_the_apps_directory() {
    let b = Machine::new();
    let repo = TempDir::new().unwrap();
    let account = repo_account_dir(repo.path());
    fs::create_dir_all(account.join("backlog")).unwrap();
    fs::write(account.join("backlog/tasks.json"), "{}").unwrap();
    fs::write(account.join("archived-sessions.idx"), "{}").unwrap();
    fs::write(account.join("local_ok.json.tmp"), "{}").unwrap();
    fs::write(
        account.join("local_ok.json"),
        serde_json::to_vec(&entry("ok", "cli-ok", "ok", 1)).unwrap(),
    )
    .unwrap();

    let plan = b.plan(repo.path());
    assert_eq!(plan.creates.len(), 1);
    assert_eq!(plan.skipped, 3, "the three other files are refused");
    b.pull(repo.path());
    assert!(b.entry_path("ok").is_file());
    assert!(!b.account_dir().join("backlog").exists());
    assert!(!b.account_dir().join("archived-sessions.idx").exists());
    assert!(!b.account_dir().join("local_ok.json.tmp").exists());
}

#[test]
fn a_session_started_on_one_machine_appears_on_the_other() {
    let a = Machine::new();
    let b = Machine::new();
    let repo = TempDir::new().unwrap();
    a.write_entry("s1", &entry("s1", "cli-1", "First", 100));

    a.push(repo.path());
    let report = b.pull(repo.path());

    assert_eq!(
        kinds(&report, ArtifactChangeKind::Added),
        vec![format!("{ORG}/{ACCOUNT}/local_s1.json")]
    );
    assert_eq!(b.read_entry("s1"), a.read_entry("s1"));
    assert!(
        b.plan(repo.path()).is_empty(),
        "a second pull has nothing to do"
    );
    // And the other way round, nothing to publish either.
    let report = b.push(repo.path());
    assert!(kinds(&report, ArtifactChangeKind::Modified).is_empty());
    assert!(kinds(&report, ArtifactChangeKind::Added).is_empty());
}

#[test]
fn an_entry_both_machines_changed_merges_field_by_field() {
    let a = Machine::new();
    let b = Machine::new();
    let repo = TempDir::new().unwrap();
    a.write_entry("s1", &entry("s1", "cli-1", "Old title", 100));
    a.sync(repo.path());
    b.sync(repo.path());

    // A keeps working in the session and renames it.
    let mut on_a = a.read_entry("s1");
    on_a["title"] = json!("New title");
    on_a["lastActivityAt"] = json!(300);
    on_a["enabledMcpTools"]["local:a-only"] = json!(true);
    a.write_entry("s1", &on_a);
    a.sync(repo.path());

    // B archived it meanwhile, which also bumped the focus time.
    let mut on_b = b.read_entry("s1");
    on_b["isArchived"] = json!(true);
    on_b["lastFocusedAt"] = json!(500);
    on_b["enabledMcpTools"]["local:b-only"] = json!(false);
    b.write_entry("s1", &on_b);

    let report = b.pull(repo.path());
    assert_eq!(
        kinds(&report, ArtifactChangeKind::Modified),
        vec![format!("{ORG}/{ACCOUNT}/local_s1.json")]
    );
    let merged = b.read_entry("s1");
    assert_eq!(merged["title"], "New title", "A's rename survives");
    assert_eq!(merged["isArchived"], true, "B's archive survives");
    assert_eq!(merged["lastActivityAt"], 300);
    assert_eq!(merged["lastFocusedAt"], 500);
    assert_eq!(
        merged["enabledMcpTools"],
        json!({"shared:tool": true, "local:a-only": true, "local:b-only": false})
    );

    // B publishes the merge; A takes it; then everything is settled.
    b.push(repo.path());
    assert_eq!(read_repo_entry(repo.path(), "s1"), merged);
    let report = a.pull(repo.path());
    assert_eq!(
        kinds(&report, ArtifactChangeKind::Modified),
        vec![format!("{ORG}/{ACCOUNT}/local_s1.json")]
    );
    assert_eq!(a.read_entry("s1"), merged);
    assert!(a.plan(repo.path()).is_empty());
    assert!(b.plan(repo.path()).is_empty());
    assert!(kinds(&a.push(repo.path()), ArtifactChangeKind::Modified).is_empty());
}

#[test]
fn without_the_kept_base_the_side_with_later_activity_wins() {
    let a = Machine::new();
    let b = Machine::new();
    let repo = TempDir::new().unwrap();
    a.write_entry("s1", &entry("s1", "cli-1", "Old title", 100));
    a.sync(repo.path());
    b.sync(repo.path());

    let mut on_a = a.read_entry("s1");
    on_a["title"] = json!("New title");
    on_a["lastActivityAt"] = json!(300);
    a.write_entry("s1", &on_a);
    a.sync(repo.path());

    let mut on_b = b.read_entry("s1");
    on_b["isArchived"] = json!(true);
    b.write_entry("s1", &on_b);
    // This machine lost its kept bases (or never had them).
    fs::remove_dir_all(b.claude.join(".claude-code-sync-desktop-bases")).unwrap();

    b.pull(repo.path());
    let merged = b.read_entry("s1");
    assert_eq!(merged["title"], "New title");
    assert_eq!(
        merged["isArchived"], false,
        "two-way: A's later activity decides a field both changed... or only B did"
    );
    assert_eq!(merged["lastActivityAt"], 300);
}

#[test]
fn a_bare_push_merges_into_the_repository_copy() {
    let a = Machine::new();
    let b = Machine::new();
    let repo = TempDir::new().unwrap();
    a.write_entry("s1", &entry("s1", "cli-1", "Old title", 100));
    a.sync(repo.path());
    b.sync(repo.path());

    let mut on_b = b.read_entry("s1");
    on_b["isArchived"] = json!(true);
    b.write_entry("s1", &on_b);
    b.push(repo.path());

    // A pushes without pulling first: the repository copy is not overwritten.
    let mut on_a = a.read_entry("s1");
    on_a["title"] = json!("New title");
    on_a["lastActivityAt"] = json!(300);
    a.write_entry("s1", &on_a);
    let report = a.push(repo.path());
    assert_eq!(
        kinds(&report, ArtifactChangeKind::Modified),
        vec![format!("{ORG}/{ACCOUNT}/local_s1.json")]
    );
    let in_repo = read_repo_entry(repo.path(), "s1");
    assert_eq!(in_repo["title"], "New title");
    assert_eq!(in_repo["isArchived"], true);

    // A's next pull takes the merge, and both settle.
    a.pull(repo.path());
    assert_eq!(a.read_entry("s1"), in_repo);
    b.pull(repo.path());
    assert_eq!(b.read_entry("s1"), in_repo);
    assert!(a.plan(repo.path()).is_empty());
    assert!(b.plan(repo.path()).is_empty());
}

#[test]
fn a_session_deleted_in_the_app_disappears_everywhere() {
    let a = Machine::new();
    let b = Machine::new();
    let repo = TempDir::new().unwrap();
    a.write_entry("s1", &entry("s1", "cli-1", "Doomed", 100));
    a.write_entry("s2", &entry("s2", "cli-2", "Kept", 100));
    a.sync(repo.path());
    b.sync(repo.path());

    // B merely opened the session since: its copy differs from the base.
    let mut on_b = b.read_entry("s1");
    on_b["lastFocusedAt"] = json!(900);
    b.write_entry("s1", &on_b);

    // A deletes it in the app: entry gone, tombstone written.
    fs::remove_file(a.entry_path("s1")).unwrap();
    a.write_tombstone("cli-1", 1_785_953_402_585);
    let report = a.push(repo.path());
    assert_eq!(
        kinds(&report, ArtifactChangeKind::Added),
        vec![format!("{ORG}/{ACCOUNT}/deleted_cli-1")]
    );
    assert_eq!(
        kinds(&report, ArtifactChangeKind::Deleted),
        vec![format!("{ORG}/{ACCOUNT}/local_s1.json")]
    );
    assert!(!repo_entry(repo.path(), "s1").exists());
    assert!(repo_entry(repo.path(), "s2").exists());

    // B's pull removes the entry regardless of its focus bump, after
    // snapshotting it, and takes the tombstone.
    let plan = b.plan(repo.path());
    assert_eq!(plan.tombstoned_deletes.len(), 1);
    assert!(plan.kept_local.is_empty() && plan.unions.is_empty());
    assert!(plan.paths_to_snapshot(false).contains(&b.entry_path("s1")));
    assert!(plan.changes_machine_state(false));
    let report = apply_pull(&plan, false).unwrap();
    assert_eq!(
        kinds(&report, ArtifactChangeKind::Deleted),
        vec![format!("{ORG}/{ACCOUNT}/local_s1.json")]
    );
    assert!(!b.entry_path("s1").exists());
    assert!(b.entry_path("s2").exists());
    assert_eq!(
        fs::read_to_string(b.tombstone_path("cli-1")).unwrap(),
        "1785953402585"
    );

    // Nothing resurrects it: not B's push, not A's pull, not a fresh machine.
    let report = b.push(repo.path());
    assert!(kinds(&report, ArtifactChangeKind::Added).is_empty());
    assert!(!repo_entry(repo.path(), "s1").exists());
    assert!(a.plan(repo.path()).is_empty());
    let c = Machine::new();
    c.pull(repo.path());
    assert!(!c.entry_path("s1").exists());
    assert!(c.entry_path("s2").exists());
    assert!(c.tombstone_path("cli-1").is_file());
}

#[test]
fn a_tombstone_removes_an_entry_only_this_machine_holds() {
    let a = Machine::new();
    let b = Machine::new();
    let repo = TempDir::new().unwrap();
    // B started the session and never pushed; A (which adopted the same
    // transcript) deleted it.
    b.write_entry("s9", &entry("s9", "cli-9", "Only here", 100));
    a.write_tombstone("cli-9", 5);
    a.push(repo.path());

    let plan = b.plan(repo.path());
    assert_eq!(plan.tombstoned_deletes.len(), 1);
    apply_pull(&plan, false).unwrap();
    assert!(!b.entry_path("s9").exists());
    assert!(b.tombstone_path("cli-9").is_file());
    // And the app on this machine lagging behind a local deletion is the
    // same thing: a local tombstone retires the local entry on pull.
    b.write_entry("s8", &entry("s8", "cli-8", "Lagging", 100));
    b.write_tombstone("cli-8", 6);
    b.pull(repo.path());
    assert!(!b.entry_path("s8").exists());
    let report = b.push(repo.path());
    assert!(kinds(&report, ArtifactChangeKind::Added)
        .iter()
        .all(|p| !p.contains("local_s8")));
}

#[test]
fn tombstones_keep_the_later_deletion_time() {
    let a = Machine::new();
    let b = Machine::new();
    let repo = TempDir::new().unwrap();
    a.write_tombstone("cli-1", 100);
    a.push(repo.path());
    b.write_tombstone("cli-1", 200);
    b.push(repo.path());
    assert_eq!(
        fs::read_to_string(repo_account_dir(repo.path()).join("deleted_cli-1")).unwrap(),
        "200"
    );
    a.pull(repo.path());
    assert_eq!(
        fs::read_to_string(a.tombstone_path("cli-1")).unwrap(),
        "200"
    );
}

#[test]
fn a_mapped_project_renders_to_each_machines_own_path() {
    let mut a = Machine::new();
    let mut b = Machine::new();
    let repo = TempDir::new().unwrap();
    let a_project = a.claude.parent().unwrap().join("work").join("app");
    let b_project = b.claude.parent().unwrap().join("src").join("app-renamed");
    a.filter
        .project_map
        .insert("app".to_string(), a_project.clone());

    let mut on_a = entry("s1", "cli-1", "Mapped", 100);
    on_a["cwd"] = json!(a_project.to_string_lossy());
    on_a["gitAnchors"] = json!([{"gitRoot": a_project.to_string_lossy()}]);
    a.write_entry("s1", &on_a);
    a.push(repo.path());
    let stored = fs::read_to_string(repo_entry(repo.path(), "s1")).unwrap();
    assert!(stored.contains("__PROJECT_app__"), "{stored}");
    assert!(!stored.contains(&a_project.to_string_lossy().to_string()));

    // Unmapped on B: skipped and named, nothing written.
    let plan = b.plan(repo.path());
    assert!(plan.creates.is_empty());
    assert_eq!(plan.skipped, 1);
    assert_eq!(
        plan.unmapped_projects.keys().collect::<Vec<_>>(),
        vec!["app"]
    );
    b.pull(repo.path());
    assert!(!b.entry_path("s1").exists());

    // Mapped on B: the entry points at B's checkout.
    b.filter
        .project_map
        .insert("app".to_string(), b_project.clone());
    b.pull(repo.path());
    let on_b = b.read_entry("s1");
    assert_eq!(on_b["cwd"], b_project.to_string_lossy().to_string());
    assert_eq!(
        on_b["gitAnchors"][0]["gitRoot"],
        b_project.to_string_lossy().to_string()
    );
    // And B's push stores it under the token again, not B's path.
    let report = b.push(repo.path());
    assert!(kinds(&report, ArtifactChangeKind::Modified).is_empty());
    assert_eq!(
        fs::read_to_string(repo_entry(repo.path(), "s1")).unwrap(),
        stored
    );
}

#[test]
fn a_machine_without_the_desktop_app_is_left_alone() {
    let a = Machine::new();
    let b = Machine::new();
    let repo = TempDir::new().unwrap();
    a.write_entry("s1", &entry("s1", "cli-1", "First", 100));
    a.push(repo.path());

    fs::remove_dir_all(&b.desktop).unwrap();
    let plan = b.plan(repo.path());
    assert!(plan.is_empty());
    assert!(!plan.changes_machine_state(false));
    b.pull(repo.path());
    assert!(
        !b.desktop.exists(),
        "the app's directory is not created for it"
    );
    let report = b.push(repo.path());
    assert!(report
        .counts
        .iter()
        .all(|c| c.category != CategoryId::DesktopSessions || c.added + c.modified == 0));
}

#[test]
fn the_category_is_off_unless_enabled() {
    let a = Machine::new();
    let repo = TempDir::new().unwrap();
    a.write_entry("s1", &entry("s1", "cli-1", "First", 100));
    let off = FilterConfig {
        sync_artifacts: ArtifactToggles::default(),
        desktop_sessions_root: Some(a.desktop.clone()),
        ..Default::default()
    };
    push_artifacts(
        &a.claude,
        repo.path(),
        &off,
        &std::collections::HashSet::new(),
    )
    .unwrap();
    assert!(!repo
        .path()
        .join("artifacts")
        .join("desktop-sessions")
        .exists());
    assert!(ArtifactToggles::all_but_hooks().desktop_sessions);
}
