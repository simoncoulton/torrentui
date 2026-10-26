//! Form state for the tag checklist and the `on_complete` action editor. Pure
//! key handling, so it is tested without a terminal; `ui.rs` draws it.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::{
    actions::{self, HookSpec},
    app::Input,
    tags,
};

/// Timeout offered for a new action, in seconds.
const DEFAULT_TIMEOUT: &str = "600";

pub struct TagRow {
    pub name: String,
    pub checked: bool,
    /// How many torrents carry the tag (0 for one only an action mentions).
    pub count: usize,
}

/// Every known tag as a checkbox, plus a field to add a new one. Unchecking a
/// tag removes it.
pub struct TagChecklist {
    /// Sorted by name.
    pub rows: Vec<TagRow>,
    /// 0 is the new-tag field; `n` is `rows[n - 1]`.
    pub cursor: usize,
    pub input: Input,
    pub error: Option<String>,
    /// A tag waiting for y/n before it is deleted everywhere.
    pub confirm_delete: Option<String>,
    /// A tag the user confirmed deleting; the owner takes it with
    /// [`TagChecklist::take_deleted`] and decides whether it may go.
    deleted: Option<String>,
}

impl TagChecklist {
    /// `known` are the tags to offer with their use counts; `checked` are
    /// ticked (and offered even when not in `known`).
    pub fn new(known: Vec<(String, usize)>, checked: &[String]) -> Self {
        let mut rows: Vec<TagRow> = Vec::new();
        for (name, count) in known {
            rows.push(TagRow {
                name,
                checked: false,
                count,
            });
        }
        for name in checked {
            match rows.iter_mut().find(|r| &r.name == name) {
                Some(r) => r.checked = true,
                None => rows.push(TagRow {
                    name: name.clone(),
                    checked: true,
                    count: 0,
                }),
            }
        }
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            // Start on the first tag when there is one, else on the field.
            cursor: usize::from(!rows.is_empty()),
            rows,
            input: Input::default(),
            error: None,
            confirm_delete: None,
            deleted: None,
        }
    }

    pub fn take_deleted(&mut self) -> Option<String> {
        self.deleted.take()
    }

    /// Drop a tag's row (after it was deleted everywhere).
    pub fn remove(&mut self, name: &str) {
        self.rows.retain(|r| r.name != name);
        self.cursor = self.cursor.min(self.rows.len());
    }

    pub fn checked(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter(|r| r.checked)
            .map(|r| r.name.clone())
            .collect()
    }

    pub fn on_input(&self) -> bool {
        self.cursor == 0
    }

    /// Add what is typed as new, checked tags (existing ones get checked).
    /// Returns whether anything was typed and accepted.
    fn add_typed(&mut self) -> bool {
        match tags::parse_tags(&self.input.text()) {
            Ok(list) => {
                for name in list {
                    match self.rows.binary_search_by(|r| r.name.cmp(&name)) {
                        Ok(i) => self.rows[i].checked = true,
                        Err(i) => self.rows.insert(
                            i,
                            TagRow {
                                name,
                                checked: true,
                                count: 0,
                            },
                        ),
                    }
                }
                self.input.clear();
                self.error = None;
                true
            }
            Err(e) => {
                self.error = Some(e);
                false
            }
        }
    }

    /// Returns true when the user asked to save: Enter on a tag, or on the
    /// field with nothing typed. Enter with text typed adds the tag instead.
    pub fn handle_key(&mut self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let typed = !self.input.text().trim().is_empty();
        if self.confirm_delete.is_some() {
            // Only `y` confirms; any other key backs out.
            if key.code == KeyCode::Char('y') {
                self.deleted = self.confirm_delete.take();
            } else {
                self.confirm_delete = None;
            }
            return false;
        }
        match key.code {
            KeyCode::Delete if !self.on_input() => {
                self.confirm_delete = Some(self.rows[self.cursor - 1].name.clone());
            }
            KeyCode::Char('d') if ctrl && !self.on_input() => {
                self.confirm_delete = Some(self.rows[self.cursor - 1].name.clone());
            }
            KeyCode::Down => self.cursor = (self.cursor + 1).min(self.rows.len()),
            KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Enter if self.on_input() && typed => {
                self.add_typed();
            }
            KeyCode::Enter => {
                // Text typed but not added yet counts too.
                return !typed || self.add_typed();
            }
            KeyCode::Char('j') if !self.on_input() && !ctrl => {
                self.cursor = (self.cursor + 1).min(self.rows.len());
            }
            KeyCode::Char('k') if !self.on_input() && !ctrl => {
                self.cursor = self.cursor.saturating_sub(1);
            }
            KeyCode::Char(' ') if !self.on_input() => {
                let row = &mut self.rows[self.cursor - 1];
                row.checked = !row.checked;
                self.error = None;
            }
            code => {
                // Typing on a tag jumps to the field so a new tag can be
                // started without moving first.
                if !self.on_input() {
                    if matches!(code, KeyCode::Char(_)) && !ctrl {
                        self.cursor = 0;
                    } else {
                        return false;
                    }
                }
                if self.input.handle_key(key) {
                    self.error = None;
                }
            }
        }
        false
    }
}

/// Which row of the editor has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Command,
    Timeout,
    Tags,
}

const FIELDS: [Field; 4] = [Field::Name, Field::Command, Field::Timeout, Field::Tags];

/// Add or edit one `on_complete` action.
pub struct HookEditor {
    /// The action being edited; `None` for a new one.
    pub index: Option<usize>,
    pub name: Input,
    pub command: Input,
    pub timeout: Input,
    pub tags: TagChecklist,
    pub focus: Field,
    pub error: Option<String>,
}

impl HookEditor {
    pub fn new(known: Vec<(String, usize)>, existing: Option<(usize, &HookSpec)>) -> Self {
        let mut name = Input::default();
        let mut command = Input::default();
        let mut timeout = Input::default();
        timeout.set(DEFAULT_TIMEOUT);
        let mut checked: &[String] = &[];
        if let Some((_, spec)) = existing {
            name.set(spec.name.as_deref().unwrap_or(""));
            command.set(&actions::join_command(&spec.run));
            timeout.set(&spec.timeout_secs.to_string());
            checked = &spec.tags;
        }
        Self {
            index: existing.map(|(i, _)| i),
            name,
            command,
            timeout,
            tags: TagChecklist::new(known, checked),
            focus: Field::Name,
            error: None,
        }
    }

    /// The text field with the keyboard, if the focus is on one.
    pub fn field(&mut self) -> Option<&mut Input> {
        match self.focus {
            Field::Name => Some(&mut self.name),
            Field::Command => Some(&mut self.command),
            Field::Timeout => Some(&mut self.timeout),
            Field::Tags => None,
        }
    }

    fn move_focus(&mut self, by: isize) {
        let at = FIELDS.iter().position(|f| *f == self.focus).unwrap_or(0) as isize;
        self.focus = FIELDS[(at + by).rem_euclid(FIELDS.len() as isize) as usize];
    }

    /// Handle a key. `Some` means the user asked to save: the finished action,
    /// or why it can't be saved yet.
    pub fn handle_key(&mut self, key: &KeyEvent) -> Option<Result<HookSpec, String>> {
        self.error = None;
        match (key.code, self.focus) {
            (KeyCode::Tab, _) => self.move_focus(1),
            (KeyCode::BackTab, _) => self.move_focus(-1),
            (KeyCode::Enter, Field::Tags) => {
                if self.tags.handle_key(key) {
                    return Some(self.build());
                }
            }
            (KeyCode::Enter, _) => return Some(self.build()),
            (KeyCode::Down, Field::Tags) => {
                self.tags.handle_key(key);
            }
            (KeyCode::Up, Field::Tags) if self.tags.cursor > 0 => {
                self.tags.handle_key(key);
            }
            (KeyCode::Down, _) => self.move_focus(1),
            (KeyCode::Up, _) => self.move_focus(-1),
            (_, Field::Tags) => {
                self.tags.handle_key(key);
            }
            (_, _) => {
                if let Some(field) = self.field() {
                    field.handle_key(key);
                }
            }
        }
        None
    }

    /// Turn the form into a validated action.
    pub fn build(&self) -> Result<HookSpec, String> {
        let run = actions::split_command(&self.command.text())?;
        if run.is_empty() {
            return Err("the command is required".into());
        }
        let timeout = self.timeout.text();
        let timeout_secs = match timeout.trim() {
            "" => DEFAULT_TIMEOUT.parse().unwrap_or(600),
            t => t
                .parse::<u64>()
                .map_err(|_| "timeout is a whole number of seconds (0 = no limit)".to_string())?,
        };
        let name = self.name.text();
        let tags = self.tags.checked();
        tags::parse_tags(&tags.join(" "))?;
        let spec = HookSpec {
            name: Some(name.trim().to_string()).filter(|n| !n.is_empty()),
            run,
            tags,
            timeout_secs,
        };
        actions::resolve(std::slice::from_ref(&spec)).map_err(|e| e.root_cause().to_string())?;
        Ok(spec)
    }
}

/// The list of actions.
#[derive(Debug, Default)]
pub struct HookManager {
    pub cursor: usize,
    /// Waiting for y/n before deleting the action under the cursor.
    pub confirm_delete: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn type_text(editor: &mut HookEditor, text: &str) {
        for c in text.chars() {
            editor.handle_key(&key(KeyCode::Char(c)));
        }
    }

    fn known() -> Vec<(String, usize)> {
        vec![("movies".into(), 2), ("tv".into(), 1)]
    }

    #[test]
    fn checklist_offers_known_and_checked_tags_sorted() {
        let list = TagChecklist::new(known(), &["tv".into(), "zzz".into()]);
        let names: Vec<_> = list.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["movies", "tv", "zzz"]);
        assert_eq!(list.checked(), ["tv", "zzz"]);
        assert_eq!(list.rows[2].count, 0);
        assert_eq!(list.cursor, 1, "starts on the first tag");
        assert_eq!(TagChecklist::new(vec![], &[]).cursor, 0);
    }

    #[test]
    fn space_toggles_and_unchecking_removes() {
        let mut list = TagChecklist::new(known(), &["movies".into()]);
        assert!(!list.handle_key(&key(KeyCode::Char(' '))));
        assert!(list.checked().is_empty());
        list.handle_key(&key(KeyCode::Down));
        list.handle_key(&key(KeyCode::Char(' ')));
        assert_eq!(list.checked(), ["tv"]);
        // The cursor stops at the ends.
        for _ in 0..5 {
            list.handle_key(&key(KeyCode::Char('j')));
        }
        assert_eq!(list.cursor, 2);
        for _ in 0..5 {
            list.handle_key(&key(KeyCode::Up));
        }
        assert_eq!(list.cursor, 0);
    }

    #[test]
    fn typing_a_new_tag_adds_it_checked_and_enter_saves_when_empty() {
        let mut list = TagChecklist::new(known(), &[]);
        // Typing while on a tag jumps to the field.
        for c in "4k Docs".chars() {
            list.handle_key(&key(KeyCode::Char(c)));
        }
        assert!(list.on_input());
        assert!(!list.handle_key(&key(KeyCode::Enter)), "adds, doesn't save");
        assert_eq!(list.checked(), ["4k", "docs"]);
        assert_eq!(list.rows.len(), 4);
        assert!(list.input.text().is_empty());
        assert!(list.handle_key(&key(KeyCode::Enter)), "empty field saves");
    }

    #[test]
    fn typing_an_existing_tag_just_checks_it() {
        let mut list = TagChecklist::new(known(), &[]);
        list.cursor = 0;
        for c in "movies".chars() {
            list.handle_key(&key(KeyCode::Char(c)));
        }
        list.handle_key(&key(KeyCode::Enter));
        assert_eq!(list.rows.len(), 2);
        assert_eq!(list.checked(), ["movies"]);
    }

    #[test]
    fn bad_tags_are_refused_with_a_message() {
        let mut list = TagChecklist::new(known(), &[]);
        list.cursor = 0;
        for c in "bad!".chars() {
            list.handle_key(&key(KeyCode::Char(c)));
        }
        assert!(!list.handle_key(&key(KeyCode::Enter)));
        assert!(list.error.as_ref().unwrap().contains("contains '!'"));
        assert_eq!(list.rows.len(), 2);
        list.handle_key(&key(KeyCode::Backspace));
        assert!(list.error.is_none());
    }

    #[test]
    fn enter_on_a_tag_saves_and_keeps_pending_text() {
        let mut list = TagChecklist::new(known(), &[]);
        list.cursor = 0;
        for c in "extra".chars() {
            list.handle_key(&key(KeyCode::Char(c)));
        }
        list.handle_key(&key(KeyCode::Down));
        assert!(list.handle_key(&key(KeyCode::Enter)));
        assert_eq!(list.checked(), ["extra"], "pending text is added on save");
    }

    #[test]
    fn editor_builds_a_valid_action() {
        let mut ed = HookEditor::new(known(), None);
        assert!(ed.handle_key(&key(KeyCode::Enter)).unwrap().is_err());
        assert_eq!(ed.error, None, "errors are reported by the caller");
        type_text(&mut ed, "sort movies");
        ed.handle_key(&key(KeyCode::Tab));
        assert_eq!(ed.focus, Field::Command);
        type_text(&mut ed, "~/bin/sort.sh '{path}' \"two words\"");
        ed.handle_key(&key(KeyCode::Down));
        assert_eq!(ed.focus, Field::Timeout);
        ed.handle_key(&key(KeyCode::Down));
        assert_eq!(ed.focus, Field::Tags);
        ed.handle_key(&key(KeyCode::Char(' ')));
        let spec = ed.handle_key(&key(KeyCode::Enter)).unwrap().unwrap();
        assert_eq!(spec.name.as_deref(), Some("sort movies"));
        assert_eq!(spec.run, ["~/bin/sort.sh", "{path}", "two words"]);
        assert_eq!(spec.tags, ["movies"]);
        assert_eq!(spec.timeout_secs, 600);
    }

    #[test]
    fn editor_rejects_bad_input() {
        let mut ed = HookEditor::new(vec![], None);
        ed.focus = Field::Command;
        type_text(&mut ed, "'unclosed");
        assert!(ed.build().unwrap_err().contains("unclosed"));
        ed.command.set("rel/path/prog");
        assert!(ed.build().unwrap_err().contains("absolute path"));
        ed.command.set("ok");
        ed.timeout.set("soon");
        assert!(ed.build().unwrap_err().contains("whole number"));
        ed.timeout.set("0");
        assert_eq!(ed.build().unwrap().timeout_secs, 0);
        ed.timeout.set("");
        assert_eq!(ed.build().unwrap().timeout_secs, 600);
        assert_eq!(ed.build().unwrap().name, None);
    }

    #[test]
    fn editing_prefills_and_up_leaves_the_tag_list() {
        let spec = HookSpec {
            name: Some("n".into()),
            run: vec!["prog".into(), "two words".into()],
            tags: vec!["tv".into()],
            timeout_secs: 30,
        };
        let mut ed = HookEditor::new(known(), Some((3, &spec)));
        assert_eq!(ed.index, Some(3));
        assert_eq!(ed.command.text(), "prog 'two words'");
        assert_eq!(ed.timeout.text(), "30");
        assert_eq!(ed.tags.checked(), ["tv"]);
        assert_eq!(ed.build().unwrap(), spec);
        ed.focus = Field::Tags;
        ed.tags.cursor = 2;
        ed.handle_key(&key(KeyCode::Up));
        ed.handle_key(&key(KeyCode::Up));
        assert_eq!((ed.focus, ed.tags.cursor), (Field::Tags, 0));
        ed.handle_key(&key(KeyCode::Up));
        assert_eq!(ed.focus, Field::Timeout);
        ed.handle_key(&key(KeyCode::BackTab));
        assert_eq!(ed.focus, Field::Command);
    }

    #[test]
    fn deleting_a_tag_needs_y_and_is_handed_to_the_owner() {
        let mut list = TagChecklist::new(known(), &[]);
        list.handle_key(&key(KeyCode::Delete));
        assert_eq!(list.confirm_delete.as_deref(), Some("movies"));
        // Anything but y backs out.
        list.handle_key(&key(KeyCode::Char('n')));
        assert!(list.confirm_delete.is_none() && list.take_deleted().is_none());
        list.handle_key(&key(KeyCode::Down));
        list.handle_key(&key(KeyCode::Delete));
        list.handle_key(&key(KeyCode::Char('y')));
        assert_eq!(list.take_deleted().as_deref(), Some("tv"));
        // The row stays until the owner accepts the deletion.
        assert_eq!(list.rows.len(), 2);
        list.remove("tv");
        assert_eq!(list.rows.len(), 1);
        assert_eq!(list.cursor, 1);
        // On the new-tag field Delete is just text editing.
        list.cursor = 0;
        list.handle_key(&key(KeyCode::Delete));
        assert!(list.confirm_delete.is_none());
    }
}
