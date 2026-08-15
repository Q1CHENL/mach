//! The `/` command palette: navigation, information, task actions, updates and quit.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommand {
    Search,
    Settings,
    Hints,
    Labels,
    Help,
    WhatsNew,
    /// Copy the selected task's title to the clipboard.
    CopyTitle,
    /// Copy the selected task's title and description to the clipboard.
    CopyTask,
    /// Export every task, category, and image to a portable archive.
    Export,
    /// Safely merge a portable archive into the current store.
    Import,
    /// Toggle whether completed tasks are shown in the list.
    Done,
    /// Permanently remove done tasks (current category, or all in All Tasks).
    Purge,
    /// Install the latest checksum-verified GitHub release.
    Update,
    Quit,
}

impl SlashCommand {
    /// Fixed palette order. Search is first when the menu opens empty.
    pub const ALL: [Self; 14] = [
        Self::Search,
        Self::Settings,
        Self::Hints,
        Self::Labels,
        Self::Help,
        Self::WhatsNew,
        Self::CopyTitle,
        Self::CopyTask,
        Self::Export,
        Self::Import,
        Self::Done,
        Self::Purge,
        Self::Update,
        Self::Quit,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Settings => "settings",
            Self::Hints => "hints",
            Self::Labels => "labels",
            Self::Help => "help",
            Self::WhatsNew => "whatsnew",
            Self::CopyTitle => "copytitle",
            Self::CopyTask => "copy",
            Self::Export => "export",
            Self::Import => "import",
            Self::Done => "done",
            Self::Purge => "purge",
            Self::Update => "update",
            Self::Quit => "quit",
        }
    }

    pub fn usage(self) -> &'static str {
        match self {
            Self::Import => "import <FILE>",
            _ => self.id(),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Search => "Search",
            Self::Settings => "Settings",
            Self::Hints => "Toggle hints",
            Self::Labels => "Labels",
            Self::Help => "Help",
            Self::WhatsNew => "What's new",
            Self::CopyTitle => "Copy title",
            Self::CopyTask => "Copy task",
            Self::Export => "Export archive",
            Self::Import => "Import archive",
            Self::Done => "Done tasks",
            Self::Purge => "Purge done",
            Self::Update => "Update",
            Self::Quit => "Quit",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::Search => "search tasks by text or label",
            Self::Settings => "sort, theme, date, preview, hints",
            Self::Hints => "toggle all or essential hints",
            Self::Labels => "create, color, rename, delete labels",
            Self::Help => "key reference",
            Self::WhatsNew => "release highlights",
            Self::CopyTitle => "copy task title",
            Self::CopyTask => "copy task and description",
            Self::Export => "save archive to ./",
            Self::Import => "merge archive from file",
            Self::Done => "show or hide completed tasks",
            Self::Purge => "delete completed tasks in this view",
            Self::Update => "install latest verified release",
            Self::Quit => "leave mach",
        }
    }

    fn keywords(self) -> &'static [&'static str] {
        match self {
            Self::Search => &["search"],
            Self::Settings => &["settings"],
            Self::Hints => &["hints"],
            Self::Labels => &["labels", "tags"],
            Self::Help => &["help"],
            Self::WhatsNew => &["whatsnew"],
            Self::CopyTitle => &["copytitle"],
            Self::CopyTask => &["copy", "copytask"],
            Self::Export => &["export", "backup"],
            Self::Import => &["import", "restore"],
            Self::Done => &["done", "hide", "show"],
            Self::Purge => &["purge"],
            Self::Update => &["update", "upgrade", "version"],
            Self::Quit => &["quit"],
        }
    }

    /// Whether this command matches the typed query (first word / prefix).
    fn matches_head(self, head: &str) -> bool {
        self.keywords().iter().any(|k| {
            // Keyword starts with what was typed ("set" → settings),
            // or typed text starts with the keyword ("search milk").
            k.starts_with(head) || (head.starts_with(k) && k.len() >= 3)
        })
    }
}

/// Commands matching `query`, in fixed palette order.
/// Empty query → all commands (Search first). Unknown text matches nothing
/// (task search is type-to-jump in the list, or `/search …` explicitly).
/// An exact keyword hit (`copy`) wins over a longer progressive match
/// (`copytitle`), so short names stay unambiguous.
pub fn matching(query: &str) -> Vec<SlashCommand> {
    let head = query.split_whitespace().next().unwrap_or("").to_lowercase();
    if head.is_empty() {
        return SlashCommand::ALL.to_vec();
    }
    let mut hits: Vec<SlashCommand> = SlashCommand::ALL
        .into_iter()
        .filter(|c| c.matches_head(&head))
        .collect();
    if hits
        .iter()
        .any(|command| command.keywords().contains(&head.as_str()))
    {
        hits.retain(|command| command.keywords().contains(&head.as_str()));
    }
    hits
}

/// Text after the command keyword, e.g. `"search milk"` → `"milk"`.
pub fn args_for(command: SlashCommand, query: &str) -> String {
    let q = query.trim();
    if q.is_empty() {
        return String::new();
    }
    let head_end = q.find(char::is_whitespace).unwrap_or(q.len());
    let (head, args) = q.split_at(head_end);
    let head = head.to_lowercase();
    if command.keywords().contains(&head.as_str()) {
        return args.trim().to_string();
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_is_first_when_unfiltered() {
        let m = matching("");
        assert_eq!(m[0], SlashCommand::Search);
        assert!(m.contains(&SlashCommand::Settings));
        assert!(m.contains(&SlashCommand::Done));
        assert!(m.contains(&SlashCommand::Purge));
        assert_eq!(m.len(), SlashCommand::ALL.len());
    }

    #[test]
    fn typing_settings_is_only_settings() {
        let m = matching("settings");
        assert_eq!(m, vec![SlashCommand::Settings]);
    }

    #[test]
    fn typing_set_is_only_settings() {
        let m = matching("set");
        assert_eq!(m, vec![SlashCommand::Settings]);
    }

    #[test]
    fn typing_hints_and_its_unique_prefix_match_hints() {
        assert_eq!(matching("hints"), vec![SlashCommand::Hints]);
        assert_eq!(matching("hint"), vec![SlashCommand::Hints]);
    }

    #[test]
    fn search_args() {
        assert_eq!(args_for(SlashCommand::Search, "search milk"), "milk");
        assert_eq!(args_for(SlashCommand::Search, "search"), "");
        assert_eq!(args_for(SlashCommand::Search, "milk"), "");
    }

    #[test]
    fn free_text_is_not_a_command() {
        assert!(matching("milk").is_empty());
    }

    #[test]
    fn typing_done_is_done_command() {
        let m = matching("done");
        assert_eq!(m, vec![SlashCommand::Done]);
    }

    #[test]
    fn typing_purge_is_only_purge() {
        let m = matching("purge");
        assert_eq!(m, vec![SlashCommand::Purge]);
        let m = matching("purge all");
        // No separate purge-all command; free-text "all" is not a keyword hit alone
        // after "purge", so only Purge matches via head "purge".
        assert_eq!(m, vec![SlashCommand::Purge]);
    }

    #[test]
    fn typing_update_is_update() {
        assert_eq!(matching("update"), vec![SlashCommand::Update]);
        assert_eq!(matching("upgrade"), vec![SlashCommand::Update]);
    }

    #[test]
    fn whats_new_is_the_only_reopenable_launch_page() {
        assert_eq!(matching("whatsnew"), vec![SlashCommand::WhatsNew]);
        assert!(matching("about").is_empty());
    }

    #[test]
    fn fixed_order_when_several_match() {
        // "s" hits search and settings; palette order keeps search first.
        let m = matching("s");
        assert_eq!(m[0], SlashCommand::Search);
        assert!(m.contains(&SlashCommand::Settings));
    }

    #[test]
    fn copy_commands_match() {
        assert_eq!(matching("copy"), vec![SlashCommand::CopyTask]);
        assert!(matching("title").is_empty());
        assert_eq!(matching("copytitle"), vec![SlashCommand::CopyTitle]);
        let m = matching("copyt");
        assert!(m.contains(&SlashCommand::CopyTitle));
        assert!(m.contains(&SlashCommand::CopyTask));
    }

    #[test]
    fn archive_commands_match_and_preserve_path_arguments() {
        assert_eq!(matching("export"), vec![SlashCommand::Export]);
        assert_eq!(matching("backup"), vec![SlashCommand::Export]);
        assert_eq!(matching("import"), vec![SlashCommand::Import]);
        assert_eq!(matching("restore"), vec![SlashCommand::Import]);
        assert_eq!(
            args_for(SlashCommand::Import, "import ~/My Tasks.mach"),
            "~/My Tasks.mach"
        );
    }

    #[test]
    fn non_ascii_command_heads_preserve_argument_boundaries() {
        assert_eq!(
            args_for(SlashCommand::Export, "bacKup /tmp/a.mach"),
            "/tmp/a.mach"
        );
        assert_eq!(args_for(SlashCommand::CopyTask, "copytasK value"), "value");
    }
}
