//! The command palette (`Ctrl+P`): everything the app can do that does not
//! earn a button or a key of its own.
//!
//! Adding a feature here costs one row in [`COMMANDS`] and one arm in
//! `KimiGuiApp::run_command` — no tab-strip real estate, no new binding.
//!
//! The boundary is deliberate and held strictly: palette commands are **GUI
//! and orchestration only** — they act on the app and its tabs (open, close,
//! resume, connect, theme, open files and folders), never on what a *session*
//! does. Anything that changes or affects the session's conversation —
//! compaction, model switching, YOLO, forking, skills, flows — is a **slash
//! command** owned by the agent (`kimi-agent`'s soul) and typed into the
//! session's input, so it works identically in every frontend. Do not add
//! session behavior here; one place per action is what keeps the two menus
//! from overlapping into confusion.

/// One palette entry. `Session` commands are hidden while no session is open,
/// since every one of them acts on the active tab.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    NewSession,
    ResumeSession,
    CloseSession,
    ConnectRemote,
    NewRemoteSession,
    OpenRemoteSession,
    CycleTheme,
    OpenConfig,
    OpenMcpConfig,
    OpenBridgeConfig,
    OpenLogFolder,
    OpenShareFolder,
    OpenWorkDir,
}

/// A palette row: what it is called, what it does, and the binding that also
/// triggers it (shown on the right, the way an editor menu does it).
pub struct Entry {
    pub command: Command,
    pub title: &'static str,
    pub detail: &'static str,
    pub binding: Option<&'static str>,
    /// Needs an active session to mean anything.
    pub needs_session: bool,
}

pub const COMMANDS: &[Entry] = &[
    Entry {
        command: Command::NewSession,
        title: "New session",
        detail: "pick a working directory",
        binding: Some("Ctrl+N"),
        needs_session: false,
    },
    Entry {
        command: Command::ResumeSession,
        title: "Resume session",
        detail: "reopen a past session",
        binding: Some("Ctrl+O"),
        needs_session: false,
    },
    Entry {
        command: Command::CloseSession,
        title: "Close session",
        detail: "close the active tab",
        binding: Some("Ctrl+T"),
        needs_session: true,
    },
    Entry {
        command: Command::ConnectRemote,
        title: "Connect to remote",
        detail: "the chain button — green opens a remote session",
        binding: None,
        needs_session: false,
    },
    Entry {
        command: Command::NewRemoteSession,
        title: "New remote session",
        detail: "a fresh agent tab on the connected daemon",
        binding: None,
        needs_session: false,
    },
    Entry {
        command: Command::OpenRemoteSession,
        title: "Open remote session",
        detail: "resume a past session from the daemon's machine",
        binding: None,
        needs_session: false,
    },
    Entry {
        command: Command::CycleTheme,
        title: "Cycle theme",
        detail: "light, dark, Kimi",
        binding: Some("Ctrl+D"),
        needs_session: false,
    },
    Entry {
        command: Command::OpenConfig,
        title: "Open config.toml",
        detail: "agent configuration, in the default editor",
        binding: None,
        needs_session: false,
    },
    Entry {
        command: Command::OpenMcpConfig,
        title: "Open mcp.json",
        detail: "MCP server configuration",
        binding: None,
        needs_session: false,
    },
    Entry {
        command: Command::OpenBridgeConfig,
        title: "Open bridge.toml",
        detail: "remote endpoints and tunnels, in the default editor",
        binding: None,
        needs_session: false,
    },
    Entry {
        command: Command::OpenLogFolder,
        title: "Open log folder",
        detail: "the agent's rolling daily logs",
        binding: None,
        needs_session: false,
    },
    Entry {
        command: Command::OpenShareFolder,
        title: "Open Kimi folder",
        detail: "~/.kimi — config, sessions, credentials",
        binding: None,
        needs_session: false,
    },
    Entry {
        command: Command::OpenWorkDir,
        title: "Open working directory",
        detail: "this session's folder, in the file manager",
        binding: None,
        needs_session: true,
    },
];

/// One filtered row: the entry, plus which of its title's char positions the
/// query actually hit — so the caller can pick those characters out visually
/// instead of just trusting the ranking.
pub struct Match {
    pub entry: &'static Entry,
    pub positions: Vec<usize>,
}

/// Open state of the palette. The matches are recomputed per frame — the list
/// is short enough that caching would cost more than it saves.
#[derive(Default)]
pub struct Palette {
    pub open: bool,
    pub query: String,
    /// Index into the *filtered* list.
    pub cursor: usize,
    /// Scroll the cursor into view on the next draw; set only by the keyboard
    /// so it never fights the mouse wheel.
    pub scroll: bool,
}

impl Palette {
    pub fn open(&mut self) {
        self.open = true;
        self.query.clear();
        self.cursor = 0;
        self.scroll = true;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.cursor = 0;
    }

    /// Commands matching the current query, best first.
    pub fn matches(&self, has_session: bool) -> Vec<Match> {
        let mut scored: Vec<(u32, usize, Match)> = COMMANDS
            .iter()
            .enumerate()
            .filter(|(_, entry)| has_session || !entry.needs_session)
            .filter_map(|(index, entry)| {
                let (score, positions) = score(&self.query, entry.title)?;
                Some((score, index, Match { entry, positions }))
            })
            .collect();
        // Ties keep declaration order, which is roughly frequency of use.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        scored.into_iter().map(|(_, _, m)| m).collect()
    }

    /// Move the highlight, clamped to the filtered list.
    pub fn step(&mut self, down: bool, len: usize) {
        let Some(last) = len.checked_sub(1) else {
            return;
        };
        self.cursor = if down {
            (self.cursor + 1).min(last)
        } else {
            self.cursor.saturating_sub(1)
        };
        self.scroll = true;
    }
}

/// Fuzzy subsequence score of `query` against `title`, or `None` when the
/// query's characters do not all appear in order. Higher is better. Also
/// returns the char positions in `title` that matched, for highlighting.
///
/// Deliberately simple — "opcfg" and "open config" both have to find *Open
/// config.toml*, and that is the whole job. Consecutive matches and matches
/// at the start of a word score higher, so a full word beats letters scraped
/// out of the middle of one.
fn score(query: &str, title: &str) -> Option<(u32, Vec<usize>)> {
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let title: Vec<char> = title.chars().flat_map(char::to_lowercase).collect();
    let mut total = 0;
    let mut at = 0;
    let mut run = 0;
    let mut positions = Vec::new();
    for needle in query.chars().flat_map(char::to_lowercase) {
        if needle.is_whitespace() {
            continue;
        }
        let found = title[at..].iter().position(|c| *c == needle)? + at;
        // A match right after the previous one, or at a word boundary, is
        // what the typist meant; an incidental letter mid-word is not.
        let boundary = found == 0 || title[found - 1] == ' ' || title[found - 1] == '.';
        run = if found == at && at > 0 { run + 1 } else { 0 };
        total += 1 + run * 4 + u32::from(boundary) * 8;
        positions.push(found);
        at = found + 1;
    }
    Some((total, positions))
}

#[cfg(test)]
mod tests {
    use super::{COMMANDS, Command, Palette, score};

    /// A palette as it looks mid-typing.
    fn with_query(query: &str) -> Palette {
        Palette {
            query: query.into(),
            ..Default::default()
        }
    }

    fn titles(palette: &Palette, has_session: bool) -> Vec<&'static str> {
        palette
            .matches(has_session)
            .into_iter()
            .map(|m| m.entry.title)
            .collect()
    }

    #[test]
    fn test_empty_query_lists_everything() {
        let palette = Palette::default();
        assert_eq!(titles(&palette, true).len(), COMMANDS.len());
    }

    #[test]
    fn test_session_commands_hidden_without_a_session() {
        let palette = Palette::default();
        let listed = titles(&palette, false);
        assert!(!listed.contains(&"Close session"));
        assert!(!listed.contains(&"Open working directory"));
        assert!(listed.contains(&"Open config.toml"));
    }

    #[test]
    fn test_words_find_the_command() {
        let palette = with_query("open config");
        assert_eq!(titles(&palette, true).first(), Some(&"Open config.toml"));
    }

    #[test]
    fn test_abbreviation_finds_the_command() {
        let palette = with_query("opcfg");
        assert_eq!(titles(&palette, true).first(), Some(&"Open config.toml"));
    }

    #[test]
    fn test_connect_finds_the_remote_command() {
        let palette = with_query("connect");
        let matches = palette.matches(false);
        assert_eq!(
            matches.first().map(|m| m.entry.command),
            Some(Command::ConnectRemote)
        );
    }

    #[test]
    fn test_bridge_finds_the_bridge_config() {
        // Reachable with no session open: the file can be edited before any
        // remote exists to connect to.
        let palette = with_query("bridge");
        assert_eq!(titles(&palette, false).first(), Some(&"Open bridge.toml"));
    }

    /// The remote commands are reachable without a session — a remote tab
    /// may be the first one this window opens.
    #[test]
    fn test_remote_commands_findable_without_a_session() {
        for (query, command) in [
            ("connect", Command::ConnectRemote),
            // Both titles contain "remote session"; ties keep declaration
            // order, so the new-session row leads.
            ("remote session", Command::NewRemoteSession),
            ("open remote", Command::OpenRemoteSession),
            ("resume", Command::ResumeSession),
        ] {
            let matches = with_query(query).matches(false);
            assert_eq!(
                matches.first().map(|m| m.entry.command),
                Some(command),
                "\"{query}\" should lead with {command:?}"
            );
        }
    }

    #[test]
    fn test_no_match_is_empty() {
        let palette = with_query("zzz");
        assert!(titles(&palette, true).is_empty());
    }

    #[test]
    fn test_word_start_beats_scraped_letters() {
        // "log" is a whole, boundary-starting word in one title and letters
        // scraped from the middle of unrelated words in the other.
        let with_word = score("log", "Open log folder").unwrap().0;
        let scraped = score("log", "xlxoxgx").unwrap().0;
        assert!(with_word > scraped);
    }

    #[test]
    fn test_out_of_order_does_not_match() {
        assert_eq!(score("gfc", "Open config.toml"), None);
    }

    /// The positions returned are what the palette highlights; they have to
    /// point at the letters that were actually typed, in title order.
    #[test]
    fn test_positions_point_at_the_matched_letters() {
        let (_, positions) = score("opcfg", "Open config.toml").unwrap();
        let title: Vec<char> = "Open config.toml".chars().collect();
        let matched: String = positions.iter().map(|&i| title[i]).collect();
        assert_eq!(matched.to_lowercase(), "opcfg");
    }

    #[test]
    fn test_close_session_is_reachable_by_its_own_name() {
        let palette = with_query("close");
        let matches = palette.matches(true);
        assert_eq!(
            matches.first().map(|m| m.entry.command),
            Some(Command::CloseSession)
        );
    }

    #[test]
    fn test_step_clamps_at_both_ends() {
        let mut palette = Palette::default();
        palette.step(false, 3);
        assert_eq!(palette.cursor, 0);
        palette.step(true, 3);
        palette.step(true, 3);
        palette.step(true, 3);
        assert_eq!(palette.cursor, 2);
    }
}
