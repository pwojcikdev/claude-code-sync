//! Machine-neutral paths: this machine's absolute locations are stored in the
//! sync repository as tokens and rendered back on the way out, so a hook
//! command or a status line still resolves on a machine whose home directory
//! or `~/.claude` location differs. Any absolute path in the file is
//! neutralized, not specific keys.
//!
//! One type owns both directions: whatever `to_machine` renders, `to_repo`
//! must undo, or two machines rewrite each other's values on every sync.
//!
//! A mapped project (see `crate::project_map`) gets a token of its own,
//! `__PROJECT_<id>__`, for the categories that opt in: a session's working
//! directory then resolves to wherever that machine keeps the project, not
//! to the same path under another home.

use std::path::Path;

use crate::project_map::ProjectMap;

/// Stands for this machine's `~`.
pub const HOME_TOKEN: &str = "__HOME__";
/// Stands for this machine's `~/.claude`. Tokenized before the home directory,
/// since it is configurable and may sit outside it.
pub const CLAUDE_DIR_TOKEN: &str = "__CLAUDE_DIR__";
/// `__PROJECT_<id>__` stands for the mapped project `<id>`, wherever this
/// machine keeps it. Tokenized before everything else, since the project
/// usually sits under the home directory.
pub const PROJECT_TOKEN_PREFIX: &str = "__PROJECT_";
const PROJECT_TOKEN_SUFFIX: &str = "__";

/// The token for a mapped project id.
pub fn project_token(id: &str) -> String {
    format!("{PROJECT_TOKEN_PREFIX}{id}{PROJECT_TOKEN_SUFFIX}")
}

/// This machine's absolute locations and their neutral spellings.
///
/// Every tokenized file is JSON, so the locations are held as they are spelled
/// inside a JSON string: on Windows `C:\Users\me` appears in the file as
/// `C:\\Users\\me`, and rendering it raw would write `\U`, an invalid escape.
#[derive(Debug, Clone, Default)]
pub struct PathTokens {
    home: String,
    claude_dir: String,
    /// Each mapped project's token and its path on this machine, longest
    /// path first so a project inside another project's tree is matched
    /// before its parent. Only applied by the `*_with_projects` methods.
    projects: Vec<(String, String)>,
}

impl PathTokens {
    /// Tokens for the machine whose Claude directory is `claude_dir`.
    pub fn for_claude_dir(claude_dir: &Path) -> Self {
        let home = dirs::home_dir()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_default();
        Self::for_paths(&home, &claude_dir.to_string_lossy())
    }

    /// Tokens for these two absolute locations, as written on this machine.
    fn for_paths(home: &str, claude_dir: &str) -> Self {
        PathTokens {
            home: json_string_contents(home),
            claude_dir: json_string_contents(claude_dir),
            projects: Vec::new(),
        }
    }

    /// The same tokens, plus one per mapped project.
    pub fn with_project_map(mut self, map: &ProjectMap) -> Self {
        let mut projects: Vec<(String, String)> = map
            .iter()
            .map(|(id, path)| {
                (
                    project_token(id),
                    json_string_contents(&path.to_string_lossy()),
                )
            })
            .collect();
        projects.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
        self.projects = projects;
        self
    }

    /// Machine bytes -> repo bytes, with mapped projects tokenized first.
    pub fn to_repo_with_projects(&self, bytes: &[u8]) -> Vec<u8> {
        let Ok(text) = std::str::from_utf8(bytes) else {
            return bytes.to_vec();
        };
        let mut text = text.to_string();
        for (token, path) in &self.projects {
            text = replace_path(&text, path, token);
        }
        self.to_repo(text.as_bytes())
    }

    /// Repo bytes -> machine bytes, rendering the mapped projects this
    /// machine knows. A token for a project it has not mapped is left as it
    /// is, for [`Self::unmapped_project`] to find.
    pub fn to_machine_with_projects(&self, bytes: &[u8]) -> Vec<u8> {
        let Ok(text) = std::str::from_utf8(bytes) else {
            return bytes.to_vec();
        };
        // Longest token first: `__PROJECT_a__` must not eat the head of
        // `__PROJECT_a__b__`.
        let mut by_token = self.projects.iter().collect::<Vec<_>>();
        by_token.sort_by_key(|entry| std::cmp::Reverse(entry.0.len()));
        let mut text = text.to_string();
        for (token, path) in by_token {
            text = text.replace(token, path);
        }
        self.to_machine(text.as_bytes())
    }

    /// The id of a project token still present in rendered bytes — one
    /// this machine has not mapped. Approximate for an id containing `__`.
    pub fn unmapped_project(bytes: &[u8]) -> Option<String> {
        let text = std::str::from_utf8(bytes).ok()?;
        let start = text.find(PROJECT_TOKEN_PREFIX)? + PROJECT_TOKEN_PREFIX.len();
        let rest = &text[start..];
        let end = rest.find(PROJECT_TOKEN_SUFFIX).unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }

    /// Machine bytes -> repo bytes. Non-UTF-8 content passes through unchanged.
    pub fn to_repo(&self, bytes: &[u8]) -> Vec<u8> {
        let Ok(text) = std::str::from_utf8(bytes) else {
            return bytes.to_vec();
        };
        let text = replace_path(text, &self.claude_dir, CLAUDE_DIR_TOKEN);
        let text = replace_path(&text, &self.home, HOME_TOKEN);
        text.into_bytes()
    }

    /// Repo bytes -> machine bytes. Non-UTF-8 content passes through unchanged.
    pub fn to_machine(&self, bytes: &[u8]) -> Vec<u8> {
        let Ok(text) = std::str::from_utf8(bytes) else {
            return bytes.to_vec();
        };
        let text = text.replace(CLAUDE_DIR_TOKEN, &self.claude_dir);
        let text = text.replace(HOME_TOKEN, &self.home);
        text.into_bytes()
    }
}

/// `text` as it is spelled between the quotes of a JSON string.
fn json_string_contents(text: &str) -> String {
    let quoted = serde_json::to_string(text).unwrap_or_default();
    quoted
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or_default()
        .to_string()
}

/// Replace `needle` with `token` only where the match is a whole path prefix:
/// `/home/user` must not eat the `/home/username` of a machine that is not this one,
/// nor the tail of an unrelated `/opt/home/user`.
fn replace_path(text: &str, needle: &str, token: &str) -> String {
    if needle.is_empty() {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(needle) {
        let before = &rest[..start];
        let after = &rest[start + needle.len()..];
        out.push_str(before);
        if continues_a_segment(before.chars().next_back())
            || continues_a_segment(after.chars().next())
        {
            out.push_str(needle);
        } else {
            out.push_str(token);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Whether a character next to a match makes it part of a longer path segment,
/// which is what tells `/home/username` and `/opt/home/user` from `/home/user` itself.
fn continues_a_segment(neighbour: Option<char>) -> bool {
    match neighbour {
        Some(character) => {
            character.is_alphanumeric() || matches!(character, '-' | '_' | '.' | '~')
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_windows_home_round_trips_as_valid_json() {
        let windows = PathTokens::for_paths(r"C:\Users\me", r"C:\Users\me\.claude");
        let live = br#"{"statusLine":{"command":"C:\\Users\\me\\.claude\\s.ps1"},"x":"C:\\Users\\me\\bin"}"#;

        let stored = String::from_utf8(windows.to_repo(live)).unwrap();
        assert!(stored.contains("__CLAUDE_DIR__"), "{stored}");
        assert!(stored.contains("__HOME__"), "{stored}");

        let from_linux = br#"{"statusLine":{"command":"__HOME__/bin/status.sh"}}"#;
        let rendered = windows.to_machine(from_linux);
        let parsed: serde_json::Value = serde_json::from_slice(&rendered).unwrap();
        assert_eq!(
            parsed["statusLine"]["command"],
            r"C:\Users\me/bin/status.sh"
        );
        assert_eq!(windows.to_machine(stored.as_bytes()), live.to_vec());
    }

    fn tokens() -> PathTokens {
        PathTokens {
            home: "/home/user".to_string(),
            claude_dir: "/home/user/.claude".to_string(),
            projects: Vec::new(),
        }
    }

    fn mapped(entries: &[(&str, &str)]) -> PathTokens {
        let map: ProjectMap = entries
            .iter()
            .map(|(id, path)| (id.to_string(), std::path::PathBuf::from(path)))
            .collect();
        tokens().with_project_map(&map)
    }

    #[test]
    fn a_mapped_project_is_tokenized_before_the_home_directory() {
        let tokens = mapped(&[("app", "/home/user/work/app")]);
        let live = br#"{"cwd":"/home/user/work/app","planPath":"/home/user/.claude/plans/x.md","other":"/home/user/work/app-renamed"}"#;
        let stored = String::from_utf8(tokens.to_repo_with_projects(live)).unwrap();
        assert_eq!(
            stored,
            r#"{"cwd":"__PROJECT_app__","planPath":"__CLAUDE_DIR__/plans/x.md","other":"__HOME__/work/app-renamed"}"#
        );
        assert_eq!(
            tokens.to_machine_with_projects(stored.as_bytes()),
            live.to_vec()
        );
        // The plain methods never see the project.
        assert!(!String::from_utf8(tokens.to_repo(live))
            .unwrap()
            .contains("__PROJECT_"));
    }

    #[test]
    fn a_project_inside_another_is_matched_first() {
        let tokens = mapped(&[
            ("outer", "/home/user/mono"),
            ("inner", "/home/user/mono/pkg"),
        ]);
        let live = br#"{"a":"/home/user/mono/pkg/src","b":"/home/user/mono/docs"}"#;
        let stored = String::from_utf8(tokens.to_repo_with_projects(live)).unwrap();
        assert_eq!(
            stored,
            r#"{"a":"__PROJECT_inner__/src","b":"__PROJECT_outer__/docs"}"#
        );
        assert_eq!(
            tokens.to_machine_with_projects(stored.as_bytes()),
            live.to_vec()
        );
    }

    #[test]
    fn a_token_this_machine_has_not_mapped_is_reported() {
        let tokens = mapped(&[("app", "/home/user/work/app")]);
        let stored = br#"{"cwd":"__PROJECT_other__/x","b":"__PROJECT_app__"}"#;
        let rendered = tokens.to_machine_with_projects(stored);
        assert_eq!(
            PathTokens::unmapped_project(&rendered),
            Some("other".to_string())
        );
        let rendered = tokens.to_machine_with_projects(br#"{"cwd":"__PROJECT_app__/x"}"#);
        assert_eq!(PathTokens::unmapped_project(&rendered), None);
        assert_eq!(
            PathTokens::unmapped_project(b"__PROJECT_unterminated"),
            Some("unterminated".to_string())
        );
    }

    #[test]
    fn tokenizes_the_claude_dir_before_the_home_directory() {
        let live = br#"{"hooks":{"Stop":[{"command":"/home/user/.claude/hooks/h.sh"}]}}"#;
        let stored = String::from_utf8(tokens().to_repo(live)).unwrap();
        assert!(stored.contains("__CLAUDE_DIR__/hooks/h.sh"), "{stored}");
        assert!(!stored.contains("/home/user"));
    }

    #[test]
    fn renders_tokens_back_to_this_machine() {
        let stored = b"node __CLAUDE_DIR__/plugins/x.js and __HOME__/bin/y";
        let live = String::from_utf8(tokens().to_machine(stored)).unwrap();
        assert_eq!(
            live,
            "node /home/user/.claude/plugins/x.js and /home/user/bin/y"
        );
    }

    #[test]
    fn a_round_trip_returns_the_original_bytes() {
        let live = br#"{"statusLine":{"command":"/home/user/bin/line --dir /home/user/.claude"}}"#;
        let stored = tokens().to_repo(live);
        assert_eq!(tokens().to_machine(&stored), live.to_vec());
    }

    #[test]
    fn another_users_home_is_left_alone() {
        let live = b"/home/username/work and /home/user/work";
        let stored = String::from_utf8(tokens().to_repo(live)).unwrap();
        assert_eq!(stored, "/home/username/work and __HOME__/work");
    }

    #[test]
    fn a_path_that_merely_contains_this_home_is_left_alone() {
        let live = b"/opt/home/user/x and /home/user/x";
        let stored = String::from_utf8(tokens().to_repo(live)).unwrap();
        assert_eq!(stored, "/opt/home/user/x and __HOME__/x");
    }

    #[test]
    fn non_utf8_content_passes_through() {
        let bytes = [0xff, 0xfe, 0x00, 0x01];
        assert_eq!(tokens().to_repo(&bytes), bytes.to_vec());
        assert_eq!(tokens().to_machine(&bytes), bytes.to_vec());
    }

    #[test]
    fn an_unknown_home_directory_changes_nothing() {
        let tokens = PathTokens {
            home: String::new(),
            claude_dir: "/srv/claude".to_string(),
            projects: Vec::new(),
        };
        let live = b"/home/user/x";
        assert_eq!(tokens.to_repo(live), live.to_vec());
    }
}
