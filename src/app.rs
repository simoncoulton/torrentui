//! Application state and input handling. No rendering and no I/O here, so it
//! can be unit-tested without a terminal or a torrent session.

use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use std::num::NonZeroU32;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::{
    actions::HookSpec,
    config::Rate,
    engine::{FileEntry, FileList, Status, TorrentView},
    forms::{HookEditor, HookManager, TagChecklist},
    proto::{PickerInfo, State},
    tags::{self, Tags},
    theme::{self, Theme},
};

const HISTORY_LEN: usize = 120;
/// How long a status message stays visible.
const STATUS_TTL: Duration = Duration::from_secs(6);
/// How long an add made in this window can wait for its torrent to appear
/// (a magnet resolves its metadata from the swarm first).
/// ...but once the daemon has nothing left to add, only this long.
const OWN_ADD_IDLE: Duration = Duration::from_secs(5);
const OWN_ADD_TTL: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Add,
    /// Confirmation before removing a torrent. With `delete_files` the
    /// downloaded data is deleted too, which has its own, stricter dialog.
    ConfirmRemove {
        id: usize,
        name: String,
        /// Where the torrent's data is saved (shown in the delete dialog).
        folder: String,
        delete_files: bool,
    },
    /// File picker; its state lives in [`App::picker`].
    Files,
    /// Asked before stopping the daemon while it is busy.
    ConfirmStop,
    /// Theme picker with live preview; state in [`App::theme_picker`].
    Theme,
    /// Speed-limit editor; state in [`App::limits_dialog`].
    Limits,
    /// Tag editor for one torrent; state in [`App::tag_editor`].
    Tags,
    /// Tag filter chooser; state in [`App::filter_picker`].
    Filter,
    /// The list of `on_complete` actions; state in [`App::hook_manager`].
    Hooks,
    /// Adding or editing one action; state in [`App::hook_editor`].
    HookEdit,
    Help,
}

/// Which torrents the list shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Filter {
    #[default]
    All,
    Untagged,
    Tag(String),
}

/// What a confirmed file selection applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerTarget {
    /// A resolved but not yet started torrent, parked in the daemon until
    /// some window answers (`token` identifies it there).
    New { token: u64, name: String },
    /// A torrent already in the session.
    Existing { id: usize },
}

pub struct FilePicker {
    pub target: PickerTarget,
    pub title: String,
    pub files: Vec<FileEntry>,
    pub checked: Vec<bool>,
    padding: Vec<usize>,
    pub cursor: usize,
}

impl FilePicker {
    /// `selected` holds the torrent file indices that start out checked.
    pub fn new(
        target: PickerTarget,
        title: String,
        list: FileList,
        selected: impl Fn(usize) -> bool,
    ) -> Self {
        let checked = list.entries.iter().map(|e| selected(e.index)).collect();
        Self {
            target,
            title,
            files: list.entries,
            checked,
            padding: list.padding,
            cursor: 0,
        }
    }

    pub fn show_progress(&self) -> bool {
        matches!(self.target, PickerTarget::Existing { .. })
    }

    pub fn selected_count(&self) -> usize {
        self.checked.iter().filter(|c| **c).count()
    }

    pub fn selected_bytes(&self) -> u64 {
        self.files
            .iter()
            .zip(&self.checked)
            .filter(|(_, c)| **c)
            .map(|(f, _)| f.len)
            .sum()
    }

    /// Torrent file indices to download, padding files included, sorted.
    pub fn chosen_indices(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self
            .files
            .iter()
            .zip(&self.checked)
            .filter(|(_, c)| **c)
            .map(|(f, _)| f.index)
            .chain(self.padding.iter().copied())
            .collect();
        v.sort_unstable();
        v
    }

    fn move_cursor(&mut self, delta: isize) {
        let last = self.files.len().saturating_sub(1) as isize;
        self.cursor = (self.cursor as isize + delta).clamp(0, last) as usize;
    }
}

/// Side effects requested by input handling; executed by the event loop.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Add a torrent, giving it these tags once it exists.
    Add {
        input: String,
        tags: Vec<String>,
    },
    TogglePause(usize),
    /// Load the file list of an existing torrent to show the picker.
    OpenFiles(usize),
    ApplyFiles {
        target: PickerTarget,
        only: Vec<usize>,
    },
    Remove {
        id: usize,
        delete_files: bool,
    },
    /// Persist the chosen theme name.
    /// `transparent` is set only when the picker changed the background
    /// setting, so an untouched one is not written to the config.
    SaveTheme {
        name: String,
        transparent: Option<bool>,
    },
    /// Apply and persist new global speed limits.
    SetLimits {
        download: Rate,
        upload: Rate,
    },
    /// Give up on a parked torrent's file selection (for every window).
    CancelPicker(u64),
    /// Run the on_complete actions for this (finished) torrent now.
    RunActions(usize),
    /// Quit and also stop the background daemon.
    StopDaemon,
    /// The `on_complete` actions changed (already applied to [`App::hooks`]):
    /// save them and make the daemon use them.
    SetHooks(Vec<HookSpec>),
    /// Delete a tag everywhere (already applied to [`App::tags`] and
    /// [`App::hooks`]).
    DeleteTag(String),
    /// Open the config file in `$EDITOR`.
    EditConfig,
    /// A torrent's tags changed (already applied to [`App::tags`]); persist them.
    SetTags {
        info_hash: String,
        tags: Vec<String>,
    },
    Quit,
}

/// Single-line text input with a char-based cursor.
#[derive(Debug, Default)]
pub struct Input {
    chars: Vec<char>,
    cursor: usize,
}

impl Input {
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }
    pub fn cursor(&self) -> usize {
        self.cursor
    }
    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }
    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars().filter(|c| !c.is_control()) {
            self.chars.insert(self.cursor, c);
            self.cursor += 1;
        }
    }
    pub fn set(&mut self, s: &str) {
        self.clear();
        self.insert_str(s);
    }

    /// Apply a text-editing key. Returns false if the key isn't an editing key.
    pub fn handle_key(&mut self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('u') if ctrl => self.clear(),
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.chars.len(),
            KeyCode::Char(c) if !ctrl => {
                let mut buf = [0u8; 4];
                self.insert_str(c.encode_utf8(&mut buf));
            }
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.chars.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.chars.len(),
            _ => return false,
        }
        true
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }
    fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }
}

pub struct ThemePicker {
    pub cursor: usize,
    /// Index of the theme in effect when the picker opened, restored on cancel.
    original: usize,
    /// Whether the background was transparent when it opened, likewise.
    original_transparent: bool,
}

pub struct LimitsDialog {
    pub download: Input,
    pub upload: Input,
    /// 0 = download, 1 = upload.
    pub focus: usize,
    pub error: Option<String>,
}

impl LimitsDialog {
    pub fn active(&mut self) -> &mut Input {
        if self.focus == 0 {
            &mut self.download
        } else {
            &mut self.upload
        }
    }
}

pub struct TagEditor {
    info_hash: String,
    pub name: String,
    pub list: TagChecklist,
    /// Opened by itself for a torrent that was just added (Esc skips it).
    pub prompt: bool,
}

pub struct FilterPicker {
    /// What can be chosen, with the number of torrents each would show.
    pub entries: Vec<(Filter, usize)>,
    pub cursor: usize,
}

/// Per-file progress of one torrent, shown in the side panel.
#[derive(Debug, Clone)]
pub struct PanelFiles {
    pub id: usize,
    pub list: FileList,
    /// Indices of the files the torrent is downloading.
    pub selected: HashSet<usize>,
}

pub struct App {
    pub mode: Mode,
    pub input: Input,
    pub torrents: Vec<TorrentView>,
    pub selected: usize,
    selected_id: Option<usize>,
    status: Option<(String, bool, Instant)>,
    pub pending_adds: usize,
    /// Whether any on_complete actions are configured (enables the `x` key).
    pub has_actions: bool,
    /// Post-download actions currently running.
    pub actions_running: usize,
    pub picker: Option<FilePicker>,
    picker_queue: VecDeque<FilePicker>,
    pub down_history: VecDeque<u64>,
    pub download_dir: String,
    pub theme: Theme,
    /// Whether the theme's background is dropped so the terminal's shows.
    pub transparent: bool,
    pub themes: Vec<Theme>,
    theme_index: usize,
    pub theme_picker: Option<ThemePicker>,
    /// Current global speed limits, for display.
    pub limits: (Rate, Rate),
    pub limits_dialog: Option<LimitsDialog>,
    /// Tags by info-hash (the daemon's copy, refreshed with every state).
    pub tags: Tags,
    /// Settings changes sent to the daemon but not yet acknowledged; while
    /// any are pending, incoming state must not overwrite the local value.
    pub settings_in_flight: usize,
    /// File selections already queued or shown, by daemon token.
    known_pickers: HashSet<u64>,
    pub filter: Filter,
    /// Indices into `torrents` that pass `filter`; `selected` indexes this.
    view: Vec<usize>,
    pub tag_editor: Option<TagEditor>,
    pub filter_picker: Option<FilterPicker>,
    /// Tags to give the torrent being added, chosen in the add dialog.
    pub add_tags: Option<TagChecklist>,
    /// Whether the add dialog's tag list (not the link field) has the keyboard.
    pub add_focus_tags: bool,
    /// Offer to tag torrents that arrive from elsewhere (`ask_tags`).
    pub ask_tags: bool,
    /// Torrent ids already seen, so only new arrivals are offered tags.
    seen_ids: Option<HashSet<usize>>,
    /// Adds started here with a tag choice, so their torrents are not
    /// prompted for again when they appear.
    own_adds: VecDeque<Instant>,
    /// New torrents waiting for their tag prompt.
    tag_prompts: VecDeque<usize>,
    /// The configured `on_complete` actions (the daemon's copy, refreshed
    /// with every state).
    pub hooks: Vec<HookSpec>,
    pub hook_manager: Option<HookManager>,
    pub hook_editor: Option<HookEditor>,
    /// Whether the per-file progress panel is open (`F`).
    pub file_panel: bool,
    pub panel_files: Option<PanelFiles>,
    /// First file row shown in the panel.
    pub panel_scroll: usize,
}

impl App {
    pub fn new(download_dir: String) -> Self {
        Self {
            mode: Mode::Normal,
            input: Input::default(),
            torrents: Vec::new(),
            selected: 0,
            selected_id: None,
            status: None,
            pending_adds: 0,
            has_actions: false,
            actions_running: 0,
            picker: None,
            picker_queue: VecDeque::new(),
            down_history: VecDeque::with_capacity(HISTORY_LEN),
            download_dir,
            theme: theme::default_theme(),
            transparent: true,
            themes: theme::builtin_themes(),
            theme_index: 0,
            theme_picker: None,
            limits: (Rate::default(), Rate::default()),
            limits_dialog: None,
            tags: Tags::new(),
            settings_in_flight: 0,
            known_pickers: HashSet::new(),
            filter: Filter::All,
            view: Vec::new(),
            tag_editor: None,
            filter_picker: None,
            add_tags: None,
            add_focus_tags: false,
            ask_tags: false,
            seen_ids: None,
            own_adds: VecDeque::new(),
            tag_prompts: VecDeque::new(),
            hooks: Vec::new(),
            hook_manager: None,
            hook_editor: None,
            file_panel: false,
            panel_files: None,
            panel_scroll: 0,
        }
    }

    /// The torrent whose files the panel needs, if it is open.
    pub fn panel_wanted(&self) -> Option<usize> {
        self.file_panel.then(|| self.current().map(|t| t.id))?
    }

    /// Store a fresh file list; one for a torrent that is no longer selected
    /// (the answer arrived late) is dropped.
    pub fn set_panel_files(&mut self, id: usize, list: FileList, selected: Vec<usize>) {
        if self.panel_wanted() != Some(id) {
            return;
        }
        if self.panel_files.as_ref().is_none_or(|p| p.id != id) {
            self.panel_scroll = 0;
        }
        self.panel_files = Some(PanelFiles {
            id,
            list,
            selected: selected.into_iter().collect(),
        });
    }

    /// Switch between the theme's background and the terminal's.
    pub fn set_transparent(&mut self, on: bool) {
        self.transparent = on;
        // The theme being previewed, if the picker is open.
        let shown = self
            .theme_picker
            .as_ref()
            .map_or(self.theme_index, |p| p.cursor);
        self.theme = self.themes[shown].clone().transparent(on);
    }

    /// Install the available themes and make `current` (by name) active.
    pub fn set_themes(&mut self, themes: Vec<Theme>, current: &str) {
        self.theme_index = themes
            .iter()
            .position(|t| theme::normalize(&t.name) == theme::normalize(current))
            .unwrap_or(0);
        self.theme = themes[self.theme_index]
            .clone()
            .transparent(self.transparent);
        self.themes = themes;
    }

    pub fn set_status(&mut self, msg: impl Into<String>, is_error: bool) {
        self.status = Some((msg.into(), is_error, Instant::now()));
    }

    /// Queue a file picker; it opens as soon as no other dialog is showing.
    pub fn queue_picker(&mut self, picker: FilePicker) {
        self.picker_queue.push_back(picker);
        self.open_next_picker();
    }

    /// Open the next queued picker if the user isn't busy in another dialog.
    pub fn open_next_picker(&mut self) {
        if self.mode == Mode::Normal
            && let Some(p) = self.picker_queue.pop_front()
        {
            self.picker = Some(p);
            self.mode = Mode::Files;
        }
    }

    pub fn clear_status(&mut self) {
        self.status = None;
    }

    /// The status message to show, as `(text, is_error)`, if still fresh.
    pub fn status(&self) -> Option<(&str, bool)> {
        self.status
            .as_ref()
            .filter(|(_, _, at)| at.elapsed() < STATUS_TTL)
            .map(|(msg, is_err, _)| (msg.as_str(), *is_err))
    }

    /// The torrent under the cursor (in the filtered list).
    pub fn current(&self) -> Option<&TorrentView> {
        self.view.get(self.selected).map(|&i| &self.torrents[i])
    }

    /// The torrents that pass the current filter, in display order.
    pub fn visible(&self) -> impl ExactSizeIterator<Item = &TorrentView> {
        self.view.iter().map(|&i| &self.torrents[i])
    }

    pub fn tags_of(&self, t: &TorrentView) -> &[String] {
        self.tags.get(&t.info_hash).map_or(&[], Vec::as_slice)
    }

    fn matches(&self, t: &TorrentView) -> bool {
        match &self.filter {
            Filter::All => true,
            Filter::Untagged => self.tags_of(t).is_empty(),
            Filter::Tag(tag) => self.tags_of(t).contains(tag),
        }
    }

    #[cfg(test)]
    pub fn set_tags(&mut self, tags: Tags) {
        self.tags = tags;
        self.refresh_view();
    }

    /// Adopt the daemon's latest state.
    pub fn apply_state(&mut self, state: State) {
        if self.settings_in_flight == 0 {
            self.tags = state.tags;
            self.hooks = state.hooks;
            self.has_actions = !self.hooks.is_empty();
            if let Some(m) = self.hook_manager.as_mut() {
                m.cursor = m.cursor.min(self.hooks.len().saturating_sub(1));
            }
            self.limits = (
                Rate(state.download_limit.and_then(NonZeroU32::new)),
                Rate(state.upload_limit.and_then(NonZeroU32::new)),
            );
        }
        self.pending_adds = state.adding;
        self.actions_running = state.actions_running;
        // After the tags: the filter depends on them.
        self.update_torrents(state.torrents);
        self.sync_pickers(state.pickers);
        // An add that failed never produces a torrent; once the daemon is
        // idle, forget our own adds that have had time to show up.
        if self.pending_adds == 0 && self.known_pickers.is_empty() {
            self.own_adds.retain(|t| t.elapsed() < OWN_ADD_IDLE);
        }
    }

    /// Show file selections other windows or the browser queued, and drop
    /// the ones that were answered elsewhere.
    fn sync_pickers(&mut self, infos: Vec<PickerInfo>) {
        let is = |p: &FilePicker, token: u64| matches!(p.target, PickerTarget::New { token: t, .. } if t == token);
        let live: HashSet<u64> = infos.iter().map(|i| i.token).collect();
        let gone: Vec<u64> = self
            .known_pickers
            .iter()
            .filter(|t| !live.contains(t))
            .copied()
            .collect();
        for token in gone {
            self.known_pickers.remove(&token);
            self.picker_queue.retain(|p| !is(p, token));
            if self.picker.as_ref().is_some_and(|p| is(p, token)) {
                self.picker = None;
                if self.mode == Mode::Files {
                    self.mode = Mode::Normal;
                }
                self.set_status("That file selection was handled in another window", false);
            }
        }
        for info in infos {
            if self.known_pickers.insert(info.token) {
                self.queue_picker(FilePicker::new(
                    PickerTarget::New {
                        token: info.token,
                        name: info.name.clone(),
                    },
                    info.name,
                    info.files,
                    |_| true,
                ));
            }
        }
    }

    /// Every tag used by a torrent in the list, with its torrent count.
    fn tag_counts(&self) -> Vec<(String, usize)> {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for t in &self.torrents {
            for tag in self.tags_of(t) {
                *counts.entry(tag).or_default() += 1;
            }
        }
        counts
            .into_iter()
            .map(|(t, n)| (t.to_string(), n))
            .collect()
    }

    /// Every tag worth offering: those on torrents (with how many) and those
    /// an action is limited to even if no torrent has them yet.
    fn known_tags(&self) -> Vec<(String, usize)> {
        let mut known: BTreeMap<String, usize> = self.tag_counts().into_iter().collect();
        for tag in self.hooks.iter().flat_map(|h| &h.tags) {
            known.entry(tag.clone()).or_default();
        }
        known.into_iter().collect()
    }

    /// Recompute the filtered list, keeping the cursor on the same torrent
    /// where possible. A filter that no longer matches anything is dropped.
    fn refresh_view(&mut self) {
        self.view = (0..self.torrents.len())
            .filter(|&i| self.matches(&self.torrents[i]))
            .collect();
        if self.view.is_empty() && self.filter != Filter::All {
            self.filter = Filter::All;
            self.set_status("Filter cleared: no torrents match it any more", false);
            self.view = (0..self.torrents.len()).collect();
        }
        let by_id = self
            .selected_id
            .and_then(|id| self.view.iter().position(|&i| self.torrents[i].id == id));
        self.selected =
            by_id.unwrap_or_else(|| self.selected.min(self.view.len().saturating_sub(1)));
        self.selected_id = self.current().map(|t| t.id);
    }

    pub fn total_down(&self) -> u64 {
        self.torrents.iter().map(|t| t.down_bps).sum()
    }

    pub fn total_up(&self) -> u64 {
        self.torrents.iter().map(|t| t.up_bps).sum()
    }

    /// Replace the torrent list, keeping the selection on the same torrent.
    pub fn update_torrents(&mut self, torrents: Vec<TorrentView>) {
        self.torrents = torrents;
        self.refresh_view();
        self.note_new_torrents();

        if self.down_history.len() == HISTORY_LEN {
            self.down_history.pop_front();
        }
        let total = self.total_down();
        self.down_history.push_back(total);
    }

    fn select(&mut self, index: usize) {
        if self.view.is_empty() {
            return;
        }
        self.selected = index.min(self.view.len() - 1);
        self.selected_id = self.current().map(|t| t.id);
    }

    pub fn handle_paste(&mut self, text: &str) {
        match self.mode {
            Mode::Add => match self.add_tags.as_mut().filter(|_| self.add_focus_tags) {
                Some(list) => {
                    list.cursor = 0;
                    list.input.insert_str(text.trim());
                }
                None => self.input.insert_str(text.trim()),
            },
            Mode::Limits => {
                if let Some(d) = self.limits_dialog.as_mut() {
                    d.active().insert_str(text.trim());
                }
            }
            Mode::Tags => {
                if let Some(e) = self.tag_editor.as_mut() {
                    e.list.cursor = 0;
                    e.list.input.insert_str(text.trim());
                    e.list.error = None;
                }
            }
            Mode::HookEdit => {
                if let Some(e) = self.hook_editor.as_mut() {
                    match e.field() {
                        Some(field) => field.insert_str(text.trim()),
                        None => {
                            e.tags.cursor = 0;
                            e.tags.input.insert_str(text.trim());
                        }
                    }
                    e.error = None;
                }
            }
            _ => {}
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(Action::Quit);
        }
        match self.mode.clone() {
            Mode::Normal => self.key_normal(key),
            Mode::Add => self.key_add(key),
            Mode::ConfirmRemove {
                id, delete_files, ..
            } => self.key_confirm(key, id, delete_files),
            Mode::Files => self.key_files(key),
            Mode::Theme => self.key_theme(key),
            Mode::Hooks => self.key_hooks(key),
            Mode::HookEdit => self.key_hook_edit(key),
            Mode::Limits => self.key_limits(key),
            Mode::ConfirmStop => self.key_confirm_stop(key),
            Mode::Tags => self.key_tags(key),
            Mode::Filter => self.key_filter(key),
            Mode::Help => {
                // Any key closes help.
                self.mode = Mode::Normal;
                None
            }
        }
    }

    /// Whether stopping the daemon would interrupt something.
    pub fn is_busy(&self) -> bool {
        self.pending_adds > 0
            || self.actions_running > 0
            || self
                .torrents
                .iter()
                .any(|t| matches!(t.status, Status::Downloading | Status::Initializing))
    }

    fn key_normal(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Char('q') => return Some(Action::Quit),
            KeyCode::Char('Q') => self.mode = Mode::ConfirmStop,
            KeyCode::Char('a') => {
                self.input.clear();
                self.add_tags = Some(TagChecklist::new(self.known_tags(), &[]));
                self.add_focus_tags = false;
                self.mode = Mode::Add;
            }
            KeyCode::Char('?') | KeyCode::F(1) => self.mode = Mode::Help,
            KeyCode::Char('T') => {
                self.theme_picker = Some(ThemePicker {
                    cursor: self.theme_index,
                    original: self.theme_index,
                    original_transparent: self.transparent,
                });
                self.mode = Mode::Theme;
            }
            KeyCode::Char('l') => {
                let fill = |r: Rate| {
                    let mut i = Input::default();
                    if r.bps().is_some() {
                        i.set(&r.to_text());
                    }
                    i
                };
                self.limits_dialog = Some(LimitsDialog {
                    download: fill(self.limits.0),
                    upload: fill(self.limits.1),
                    focus: 0,
                    error: None,
                });
                self.mode = Mode::Limits;
            }
            KeyCode::Char('x') => return self.run_actions_key(),
            KeyCode::Char('t') => self.open_tag_editor(),
            KeyCode::Char('/') => self.open_filter_picker(),
            KeyCode::Char('E') => return Some(Action::EditConfig),
            KeyCode::Char('A') => {
                self.hook_manager = Some(HookManager::default());
                self.mode = Mode::Hooks;
            }
            KeyCode::Char('F') => {
                self.file_panel = !self.file_panel;
                self.panel_files = None;
                self.panel_scroll = 0;
            }
            KeyCode::PageDown if self.file_panel => self.panel_scroll += 10,
            KeyCode::PageUp if self.file_panel => {
                self.panel_scroll = self.panel_scroll.saturating_sub(10)
            }
            KeyCode::Esc if self.filter != Filter::All => {
                self.filter = Filter::All;
                self.refresh_view();
            }
            KeyCode::Down | KeyCode::Char('j') => self.select(self.selected + 1),
            KeyCode::Up | KeyCode::Char('k') => self.select(self.selected.saturating_sub(1)),
            KeyCode::Home | KeyCode::Char('g') => self.select(0),
            KeyCode::End | KeyCode::Char('G') => self.select(usize::MAX),
            KeyCode::Char('p') | KeyCode::Char(' ') => {
                return self.current().map(|t| Action::TogglePause(t.id));
            }
            KeyCode::Char('f') | KeyCode::Enter => {
                return self.current().map(|t| Action::OpenFiles(t.id));
            }
            KeyCode::Char('d') | KeyCode::Delete => self.ask_remove(false),
            KeyCode::Char('D') => self.ask_remove(true),
            _ => {}
        }
        None
    }

    fn key_add(&mut self, key: KeyEvent) -> Option<Action> {
        let Some(list) = self.add_tags.as_mut() else {
            self.mode = Mode::Normal;
            return None;
        };
        let focus_tags = self.add_focus_tags;
        match key.code {
            KeyCode::Esc if list.confirm_delete.is_none() => {
                self.add_tags = None;
                self.mode = Mode::Normal;
            }
            KeyCode::Tab | KeyCode::BackTab => self.add_focus_tags = !focus_tags,
            KeyCode::Down if !focus_tags => self.add_focus_tags = true,
            KeyCode::Up if focus_tags && list.cursor == 0 && list.confirm_delete.is_none() => {
                self.add_focus_tags = false;
            }
            KeyCode::Enter if !focus_tags => return self.submit_add(),
            _ if focus_tags => {
                let save = list.handle_key(&key);
                if let Some(tag) = list.take_deleted() {
                    let result = self.delete_tag_everywhere(&tag);
                    let list = self.add_tags.as_mut()?;
                    match result {
                        Ok(action) => {
                            list.remove(&tag);
                            return Some(action);
                        }
                        Err(e) => list.error = Some(e),
                    }
                } else if save {
                    return self.submit_add();
                }
            }
            _ => {
                self.input.handle_key(&key);
            }
        }
        None
    }

    /// Send what the add dialog holds. An empty link just closes it.
    fn submit_add(&mut self) -> Option<Action> {
        let text = self.input.text();
        let list = self.add_tags.as_mut()?;
        let tags = list.checked();
        if let Err(e) = tags::parse_tags(&tags.join(" ")) {
            list.error = Some(e);
            return None;
        }
        self.add_tags = None;
        self.mode = Mode::Normal;
        self.input.clear();
        if text.trim().is_empty() {
            return None;
        }
        // The tag choice was made here; don't ask again when it arrives.
        self.own_adds.push_back(Instant::now());
        Some(Action::Add { input: text, tags })
    }

    fn key_theme(&mut self, key: KeyEvent) -> Option<Action> {
        let Some(picker) = self.theme_picker.as_mut() else {
            self.mode = Mode::Normal;
            return None;
        };
        let last = self.themes.len().saturating_sub(1);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => picker.cursor = (picker.cursor + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => picker.cursor = picker.cursor.saturating_sub(1),
            KeyCode::Home | KeyCode::Char('g') => picker.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => picker.cursor = last,
            KeyCode::Char('b') => {
                let on = !self.transparent;
                self.set_transparent(on);
                return None;
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                let (original, was) = (picker.original, picker.original_transparent);
                self.theme_picker = None;
                self.transparent = was;
                self.theme = self.themes[original].clone().transparent(was);
                self.mode = Mode::Normal;
                return None;
            }
            KeyCode::Enter => {
                let (cursor, was) = (picker.cursor, picker.original_transparent);
                self.theme_picker = None;
                self.theme_index = cursor;
                self.theme = self.themes[cursor].clone().transparent(self.transparent);
                self.mode = Mode::Normal;
                return Some(Action::SaveTheme {
                    name: self.theme.name.clone(),
                    transparent: (self.transparent != was).then_some(self.transparent),
                });
            }
            _ => return None,
        }
        // Live preview of the highlighted theme.
        self.theme = self.themes[picker.cursor]
            .clone()
            .transparent(self.transparent);
        None
    }

    fn key_limits(&mut self, key: KeyEvent) -> Option<Action> {
        let Some(dialog) = self.limits_dialog.as_mut() else {
            self.mode = Mode::Normal;
            return None;
        };
        match key.code {
            KeyCode::Esc => {
                self.limits_dialog = None;
                self.mode = Mode::Normal;
            }
            KeyCode::Tab | KeyCode::Down => dialog.focus = 1,
            KeyCode::BackTab | KeyCode::Up => dialog.focus = 0,
            KeyCode::Enter => {
                let parsed = (
                    dialog.download.text().parse::<Rate>(),
                    dialog.upload.text().parse::<Rate>(),
                );
                match parsed {
                    (Ok(download), Ok(upload)) => {
                        self.limits = (download, upload);
                        self.limits_dialog = None;
                        self.mode = Mode::Normal;
                        return Some(Action::SetLimits { download, upload });
                    }
                    (Err(e), _) => {
                        dialog.focus = 0;
                        dialog.error = Some(format!("Download: {e}"));
                    }
                    (_, Err(e)) => {
                        dialog.focus = 1;
                        dialog.error = Some(format!("Upload: {e}"));
                    }
                }
            }
            _ => {
                if dialog.active().handle_key(&key) {
                    dialog.error = None;
                }
            }
        }
        None
    }

    fn run_actions_key(&mut self) -> Option<Action> {
        let t = self.current()?;
        if !self.has_actions {
            self.set_status(
                "No on_complete actions are configured (see the config file)",
                true,
            );
            return None;
        }
        let finished =
            t.status == Status::Seeding || (t.total_bytes > 0 && t.progress_bytes >= t.total_bytes);
        if !finished {
            self.set_status(
                "Actions run on finished torrents; this one is still downloading",
                true,
            );
            return None;
        }
        Some(Action::RunActions(t.id))
    }

    fn open_tag_editor(&mut self) {
        let Some(t) = self.current() else { return };
        let (info_hash, name) = (t.info_hash.clone(), t.name.clone());
        self.open_tags_for(info_hash, name, false);
    }

    fn open_tags_for(&mut self, info_hash: String, name: String, prompt: bool) {
        let current = self.tags.get(&info_hash).cloned().unwrap_or_default();
        // Every tag in use is offered so tags stay consistent across torrents.
        let list = TagChecklist::new(self.known_tags(), &current);
        self.tag_editor = Some(TagEditor {
            info_hash,
            name,
            list,
            prompt,
        });
        self.mode = Mode::Tags;
    }

    /// Offer tags for the next torrent that arrived from elsewhere (a browser
    /// link, another window), once the user is out of other dialogs.
    pub fn open_next_tag_prompt(&mut self) {
        if self.mode != Mode::Normal || self.tag_editor.is_some() {
            return;
        }
        while let Some(id) = self.tag_prompts.pop_front() {
            let Some(t) = self.torrents.iter().find(|t| t.id == id) else {
                continue;
            };
            // Tagged in the meantime (another window, the add dialog).
            if self.tags.get(&t.info_hash).is_some_and(|v| !v.is_empty()) {
                continue;
            }
            let (info_hash, name) = (t.info_hash.clone(), t.name.clone());
            self.open_tags_for(info_hash, name, true);
            return;
        }
    }

    /// Queue a tag prompt for each torrent not seen before.
    fn note_new_torrents(&mut self) {
        let ids: HashSet<usize> = self.torrents.iter().map(|t| t.id).collect();
        let Some(seen) = self.seen_ids.as_mut() else {
            // The first state is what was already there.
            self.seen_ids = Some(ids);
            return;
        };
        let fresh: Vec<usize> = self
            .torrents
            .iter()
            .filter(|t| seen.insert(t.id))
            .map(|t| t.id)
            .collect();
        for id in fresh {
            while self
                .own_adds
                .front()
                .is_some_and(|t| t.elapsed() > OWN_ADD_TTL)
            {
                self.own_adds.pop_front();
            }
            if self.own_adds.pop_front().is_some() || !self.ask_tags {
                continue;
            }
            self.tag_prompts.push_back(id);
        }
    }

    fn key_tags(&mut self, key: KeyEvent) -> Option<Action> {
        let Some(editor) = self.tag_editor.as_mut() else {
            self.mode = Mode::Normal;
            return None;
        };
        // Esc backs out of a pending delete before it closes the editor.
        if key.code == KeyCode::Esc && editor.list.confirm_delete.is_none() {
            self.tag_editor = None;
            self.mode = Mode::Normal;
            return None;
        }
        let save = editor.list.handle_key(&key);
        if let Some(tag) = editor.list.take_deleted() {
            let result = self.delete_tag_everywhere(&tag);
            let list = &mut self.tag_editor.as_mut()?.list;
            return match result {
                Ok(action) => {
                    list.remove(&tag);
                    Some(action)
                }
                Err(e) => {
                    list.error = Some(e);
                    None
                }
            };
        }
        if !save {
            return None;
        }
        match tags::parse_tags(&editor.list.checked().join(" ")) {
            Ok(list) => {
                let info_hash = editor.info_hash.clone();
                self.tag_editor = None;
                self.mode = Mode::Normal;
                if list.is_empty() {
                    self.tags.remove(&info_hash);
                } else {
                    self.tags.insert(info_hash.clone(), list.clone());
                }
                self.refresh_view();
                Some(Action::SetTags {
                    info_hash,
                    tags: list,
                })
            }
            Err(e) => {
                editor.list.error = Some(e);
                None
            }
        }
    }

    /// Remove a tag from every torrent and action here, ahead of the daemon
    /// doing the same. An action limited to only this tag would start running
    /// for every torrent, so that is refused.
    fn delete_tag_everywhere(&mut self, tag: &str) -> Result<Action, String> {
        if let Some(h) = self.hooks.iter().find(|h| h.tags == [tag]) {
            let label = h
                .name
                .clone()
                .unwrap_or_else(|| crate::actions::join_command(&h.run));
            return Err(format!(
                "Action \"{label}\" only runs for #{tag}: edit or delete it first"
            ));
        }
        for list in self.tags.values_mut() {
            list.retain(|t| t != tag);
        }
        self.tags.retain(|_, list| !list.is_empty());
        for h in &mut self.hooks {
            h.tags.retain(|t| t != tag);
        }
        self.refresh_view();
        self.set_status(format!("Deleted tag #{tag}"), false);
        Ok(Action::DeleteTag(tag.to_string()))
    }

    fn key_hooks(&mut self, key: KeyEvent) -> Option<Action> {
        let Some(manager) = self.hook_manager.as_mut() else {
            self.mode = Mode::Normal;
            return None;
        };
        let last = self.hooks.len().saturating_sub(1);
        if manager.confirm_delete {
            match key.code {
                KeyCode::Char('y') => {
                    manager.confirm_delete = false;
                    if manager.cursor < self.hooks.len() {
                        self.hooks.remove(manager.cursor);
                        manager.cursor = manager.cursor.min(self.hooks.len().saturating_sub(1));
                        self.has_actions = !self.hooks.is_empty();
                        return Some(Action::SetHooks(self.hooks.clone()));
                    }
                }
                KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => {
                    manager.confirm_delete = false;
                }
                _ => {}
            }
            return None;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.hook_manager = None;
                self.mode = Mode::Normal;
            }
            KeyCode::Down | KeyCode::Char('j') => manager.cursor = (manager.cursor + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => manager.cursor = manager.cursor.saturating_sub(1),
            KeyCode::Char('a') => {
                self.hook_editor = Some(HookEditor::new(self.known_tags(), None));
                self.mode = Mode::HookEdit;
            }
            KeyCode::Enter | KeyCode::Char('e') => {
                let index = manager.cursor;
                if let Some(spec) = self.hooks.get(index) {
                    self.hook_editor =
                        Some(HookEditor::new(self.known_tags(), Some((index, spec))));
                    self.mode = Mode::HookEdit;
                }
            }
            KeyCode::Char('d') | KeyCode::Delete if !self.hooks.is_empty() => {
                manager.confirm_delete = true;
            }
            _ => {}
        }
        None
    }

    fn key_hook_edit(&mut self, key: KeyEvent) -> Option<Action> {
        let Some(editor) = self.hook_editor.as_mut() else {
            self.mode = Mode::Hooks;
            return None;
        };
        if key.code == KeyCode::Esc && editor.tags.confirm_delete.is_none() {
            self.hook_editor = None;
            self.mode = Mode::Hooks;
            return None;
        }
        let outcome = editor.handle_key(&key);
        if let Some(tag) = editor.tags.take_deleted() {
            let result = self.delete_tag_everywhere(&tag);
            let editor = self.hook_editor.as_mut()?;
            return match result {
                Ok(action) => {
                    editor.tags.remove(&tag);
                    Some(action)
                }
                Err(e) => {
                    editor.error = Some(e);
                    None
                }
            };
        }
        match outcome? {
            Err(e) => {
                editor.error = Some(e);
                None
            }
            Ok(spec) => {
                let at = match editor.index.filter(|&i| i < self.hooks.len()) {
                    Some(i) => {
                        self.hooks[i] = spec;
                        i
                    }
                    None => {
                        self.hooks.push(spec);
                        self.hooks.len() - 1
                    }
                };
                self.hook_editor = None;
                self.has_actions = true;
                self.hook_manager = Some(HookManager {
                    cursor: at,
                    confirm_delete: false,
                });
                self.mode = Mode::Hooks;
                Some(Action::SetHooks(self.hooks.clone()))
            }
        }
    }

    fn open_filter_picker(&mut self) {
        let counts = self.tag_counts();
        if counts.is_empty() {
            self.set_status("No tags yet: press T to tag the selected torrent", false);
            return;
        }
        let untagged = self
            .torrents
            .iter()
            .filter(|t| self.tags_of(t).is_empty())
            .count();
        let mut entries = vec![
            (Filter::All, self.torrents.len()),
            (Filter::Untagged, untagged),
        ];
        entries.extend(counts.into_iter().map(|(t, n)| (Filter::Tag(t), n)));
        let cursor = entries
            .iter()
            .position(|(f, _)| *f == self.filter)
            .unwrap_or(0);
        self.filter_picker = Some(FilterPicker { entries, cursor });
        self.mode = Mode::Filter;
    }

    fn key_filter(&mut self, key: KeyEvent) -> Option<Action> {
        let Some(picker) = self.filter_picker.as_mut() else {
            self.mode = Mode::Normal;
            return None;
        };
        let last = picker.entries.len().saturating_sub(1);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => picker.cursor = (picker.cursor + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => picker.cursor = picker.cursor.saturating_sub(1),
            KeyCode::Home | KeyCode::Char('g') => picker.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => picker.cursor = last,
            KeyCode::Esc | KeyCode::Char('q') => {
                self.filter_picker = None;
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => {
                let picker = self.filter_picker.take()?;
                self.mode = Mode::Normal;
                if let Some((filter, n)) = picker.entries.into_iter().nth(picker.cursor) {
                    // Counts were taken when the picker opened; don't apply a
                    // filter that has since become empty.
                    if n > 0 || filter == Filter::All {
                        self.filter = filter;
                        self.refresh_view();
                    }
                }
            }
            _ => {}
        }
        None
    }

    fn key_files(&mut self, key: KeyEvent) -> Option<Action> {
        let Some(picker) = self.picker.as_mut() else {
            self.mode = Mode::Normal;
            return None;
        };
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => picker.move_cursor(1),
            KeyCode::Up | KeyCode::Char('k') => picker.move_cursor(-1),
            KeyCode::PageDown => picker.move_cursor(10),
            KeyCode::PageUp => picker.move_cursor(-10),
            KeyCode::Home | KeyCode::Char('g') => picker.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => {
                picker.cursor = picker.files.len().saturating_sub(1)
            }
            KeyCode::Char(' ') => {
                if let Some(c) = picker.checked.get_mut(picker.cursor) {
                    *c = !*c;
                }
            }
            KeyCode::Char('a') => picker.checked.iter_mut().for_each(|c| *c = true),
            KeyCode::Char('n') => picker.checked.iter_mut().for_each(|c| *c = false),
            KeyCode::Char('i') => picker.checked.iter_mut().for_each(|c| *c = !*c),
            KeyCode::Esc | KeyCode::Char('q') => {
                let picker = self.picker.take()?;
                self.mode = Mode::Normal;
                self.open_next_picker();
                if let PickerTarget::New { token, .. } = picker.target {
                    return Some(Action::CancelPicker(token));
                }
            }
            KeyCode::Enter => {
                if picker.selected_count() == 0 {
                    self.set_status("Select at least one file (Esc cancels)", true);
                    return None;
                }
                let picker = self.picker.take()?;
                self.mode = Mode::Normal;
                let only = picker.chosen_indices();
                self.open_next_picker();
                return Some(Action::ApplyFiles {
                    target: picker.target,
                    only,
                });
            }
            _ => {}
        }
        None
    }

    fn key_confirm_stop(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Char('y') => {
                self.mode = Mode::Normal;
                Some(Action::StopDaemon)
            }
            KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Normal;
                None
            }
            _ => None,
        }
    }

    fn ask_remove(&mut self, delete_files: bool) {
        if let Some(t) = self.current() {
            self.mode = Mode::ConfirmRemove {
                id: t.id,
                name: t.name.clone(),
                folder: t.output_folder.display().to_string(),
                delete_files,
            };
        }
    }

    /// Nothing is ever deleted by a single keypress from the list: removing
    /// needs `y` in a dialog, and deleting data needs `y` in a separate,
    /// red dialog (Enter does not confirm that one).
    fn key_confirm(&mut self, key: KeyEvent, id: usize, delete_files: bool) -> Option<Action> {
        match key.code {
            KeyCode::Char('y') => {}
            KeyCode::Enter if !delete_files => {}
            KeyCode::Char('D') if !delete_files => {
                // Escalate to the delete dialog; this does not delete yet.
                if let Mode::ConfirmRemove { delete_files, .. } = &mut self.mode {
                    *delete_files = true;
                }
                return None;
            }
            KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Normal;
                return None;
            }
            _ => return None, // ignore other keys, stay in the dialog
        }
        self.mode = Mode::Normal;
        Some(Action::Remove { id, delete_files })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Status;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn view(id: usize) -> TorrentView {
        TorrentView {
            id,
            name: format!("t{id}"),
            info_hash: String::new(),
            output_folder: Default::default(),
            status: Status::Downloading,
            progress_bytes: 0,
            total_bytes: 0,
            uploaded_bytes: 0,
            down_bps: 10,
            up_bps: 5,
            peers_live: 0,
            peers_connecting: 0,
            peers_queued: 0,
            peers_seen: 0,
            eta: None,
            error: None,
        }
    }

    #[test]
    fn add_flow_returns_action() {
        let mut app = App::new(String::new());
        assert_eq!(app.handle_key(key(KeyCode::Char('a'))), None);
        assert_eq!(app.mode, Mode::Add);
        app.handle_paste("magnet:?xt=urn:btih:abc\n");
        let a = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            a,
            Some(Action::Add {
                input: "magnet:?xt=urn:btih:abc".into(),
                tags: vec![]
            })
        );
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn empty_add_is_ignored_and_esc_cancels() {
        let mut app = App::new(String::new());
        app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn typing_q_in_add_mode_does_not_quit() {
        let mut app = App::new(String::new());
        app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), None);
        assert_eq!(app.input.text(), "q");
    }

    #[test]
    fn input_editing() {
        let mut app = App::new(String::new());
        app.handle_key(key(KeyCode::Char('a')));
        for c in "abc".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.input.text(), "ac");
        app.handle_key(key(KeyCode::Delete));
        assert_eq!(app.input.text(), "a");
        app.handle_key(key(KeyCode::Home));
        app.handle_key(key(KeyCode::Char('é')));
        assert_eq!(app.input.text(), "éa");
    }

    #[test]
    fn selection_follows_torrent_id() {
        let mut app = App::new(String::new());
        app.update_torrents(vec![view(1), view(2), view(3)]);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.current().unwrap().id, 2);
        // torrent 1 removed: selection stays on id 2
        app.update_torrents(vec![view(2), view(3)]);
        assert_eq!(app.current().unwrap().id, 2);
        // selected torrent removed: clamps
        app.update_torrents(vec![view(3)]);
        assert_eq!(app.current().unwrap().id, 3);
        app.update_torrents(vec![]);
        assert!(app.current().is_none());
        assert_eq!(app.handle_key(key(KeyCode::Char('p'))), None);
    }

    #[test]
    fn navigation_clamps() {
        let mut app = App::new(String::new());
        app.update_torrents(vec![view(1), view(2)]);
        app.handle_key(key(KeyCode::Char('G')));
        assert_eq!(app.selected, 1);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.selected, 1);
        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn remove_keeps_files_after_one_confirmation() {
        let mut app = App::new(String::new());
        app.update_torrents(vec![view(7)]);
        assert_eq!(app.handle_key(key(KeyCode::Char('d'))), None);
        assert!(matches!(
            app.mode,
            Mode::ConfirmRemove {
                id: 7,
                delete_files: false,
                ..
            }
        ));
        // Unrelated keys are ignored and the dialog stays.
        assert_eq!(app.handle_key(key(KeyCode::Char('x'))), None);
        assert!(matches!(app.mode, Mode::ConfirmRemove { .. }));
        for confirm in [KeyCode::Char('y'), KeyCode::Enter] {
            app.mode = Mode::Normal;
            app.handle_key(key(KeyCode::Char('d')));
            assert_eq!(
                app.handle_key(key(confirm)),
                Some(Action::Remove {
                    id: 7,
                    delete_files: false
                })
            );
            assert_eq!(app.mode, Mode::Normal);
        }
        for cancel in [KeyCode::Char('n'), KeyCode::Esc, KeyCode::Char('q')] {
            app.handle_key(key(KeyCode::Char('d')));
            assert_eq!(app.handle_key(key(cancel)), None);
            assert_eq!(app.mode, Mode::Normal);
        }
    }

    #[test]
    fn deleting_data_needs_its_own_explicit_confirmation() {
        let mut v = view(7);
        v.output_folder = "/dl/Movie".into();
        let mut app = App::new(String::new());
        app.update_torrents(vec![v]);

        // `D` from the list only opens the delete dialog.
        assert_eq!(app.handle_key(key(KeyCode::Char('D'))), None);
        let Mode::ConfirmRemove {
            folder,
            delete_files,
            ..
        } = app.mode.clone()
        else {
            panic!("expected the delete dialog");
        };
        assert!(delete_files);
        assert_eq!(folder, "/dl/Movie");
        // Enter and D do not confirm a deletion; only `y` does.
        for k in [KeyCode::Enter, KeyCode::Char('D'), KeyCode::Char(' ')] {
            assert_eq!(app.handle_key(key(k)), None);
            assert!(matches!(
                app.mode,
                Mode::ConfirmRemove {
                    delete_files: true,
                    ..
                }
            ));
        }
        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            Some(Action::Remove {
                id: 7,
                delete_files: true
            })
        );
        assert_eq!(app.mode, Mode::Normal);

        // Esc cancels everything, removing nothing.
        app.handle_key(key(KeyCode::Char('D')));
        assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn d_then_capital_d_escalates_but_never_deletes_directly() {
        let mut app = App::new(String::new());
        app.update_torrents(vec![view(7)]);
        app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(app.handle_key(key(KeyCode::Char('D'))), None);
        assert!(matches!(
            app.mode,
            Mode::ConfirmRemove {
                delete_files: true,
                ..
            }
        ));
        // Enter no longer removes: it is now the strict dialog.
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert!(matches!(app.mode, Mode::ConfirmRemove { .. }));
    }

    #[test]
    fn remove_keys_do_nothing_without_a_torrent() {
        let mut app = App::new(String::new());
        app.handle_key(key(KeyCode::Char('d')));
        app.handle_key(key(KeyCode::Char('D')));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn ctrl_c_quits_from_any_mode() {
        let mut app = App::new(String::new());
        app.handle_key(key(KeyCode::Char('a')));
        let mut k = key(KeyCode::Char('c'));
        k.modifiers = KeyModifiers::CONTROL;
        assert_eq!(app.handle_key(k), Some(Action::Quit));
    }

    fn entries(n: usize) -> FileList {
        FileList {
            entries: (0..n)
                .map(|i| FileEntry {
                    // indices skip 1 to mimic a hidden padding file at index 1
                    index: if i == 0 { 0 } else { i + 1 },
                    path: format!("f{i}"),
                    len: 100,
                    done: 0,
                })
                .collect(),
            padding: vec![1],
        }
    }

    fn new_picker(app: &mut App, n: usize) {
        app.queue_picker(FilePicker::new(
            PickerTarget::New {
                token: 9,
                name: "t".into(),
            },
            "t".into(),
            entries(n),
            |_| true,
        ));
    }

    #[test]
    fn picker_selects_and_applies_with_padding() {
        let mut app = App::new(String::new());
        new_picker(&mut app, 3);
        assert_eq!(app.mode, Mode::Files);
        app.handle_key(key(KeyCode::Char('n'))); // none
        assert_eq!(app.picker.as_ref().unwrap().selected_count(), 0);
        // Enter with nothing selected is refused and keeps the picker open
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(app.mode, Mode::Files);
        assert!(app.status().is_some_and(|(_, err)| err));

        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Char(' '))); // check f1 (index 2)
        assert_eq!(app.picker.as_ref().unwrap().selected_bytes(), 100);
        let action = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            action,
            Some(Action::ApplyFiles {
                target: PickerTarget::New {
                    token: 9,
                    name: "t".into()
                },
                only: vec![1, 2], // padding index 1 is always kept
            })
        );
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.picker.is_none());
    }

    #[test]
    fn picker_invert_cancel_and_cursor_bounds() {
        let mut app = App::new(String::new());
        new_picker(&mut app, 2);
        app.handle_key(key(KeyCode::Char('i')));
        assert_eq!(app.picker.as_ref().unwrap().selected_count(), 0);
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.picker.as_ref().unwrap().cursor, 1);
        app.handle_key(key(KeyCode::PageUp));
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.picker.as_ref().unwrap().cursor, 0);
        // Cancelling a parked torrent is reported to the daemon, which
        // releases it for every window.
        assert_eq!(
            app.handle_key(key(KeyCode::Esc)),
            Some(Action::CancelPicker(9))
        );
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn pickers_queue_and_wait_for_other_dialogs() {
        let mut app = App::new(String::new());
        app.handle_key(key(KeyCode::Char('a'))); // typing a magnet
        new_picker(&mut app, 2);
        assert_eq!(app.mode, Mode::Add, "must not steal focus");
        app.handle_key(key(KeyCode::Esc));
        app.open_next_picker();
        assert_eq!(app.mode, Mode::Files);

        new_picker(&mut app, 2); // second one waits
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Files, "next queued picker opens");
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn existing_picker_keeps_current_selection() {
        let mut app = App::new(String::new());
        app.update_torrents(vec![view(5)]);
        assert_eq!(
            app.handle_key(key(KeyCode::Char('f'))),
            Some(Action::OpenFiles(5))
        );
        app.queue_picker(FilePicker::new(
            PickerTarget::Existing { id: 5 },
            "t".into(),
            entries(3),
            |i| i == 0,
        ));
        assert_eq!(app.picker.as_ref().unwrap().selected_count(), 1);
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Action::ApplyFiles {
                target: PickerTarget::Existing { id: 5 },
                only: vec![0, 1],
            })
        );
    }

    fn themed_app() -> App {
        let mut app = App::new(String::new());
        app.set_themes(theme::builtin_themes(), "default");
        app
    }

    #[test]
    fn theme_picker_previews_saves_and_cancels() {
        let mut app = themed_app();
        assert_eq!(app.theme.name, "default");
        app.handle_key(key(KeyCode::Char('T')));
        assert_eq!(app.mode, Mode::Theme);
        app.handle_key(key(KeyCode::Down));
        let previewed = app.theme.name.clone();
        assert_ne!(previewed, "default", "moving previews the theme live");

        // cancel restores the original
        assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
        assert_eq!(app.theme.name, "default");
        assert_eq!(app.mode, Mode::Normal);

        // confirm keeps it and asks for it to be saved
        app.handle_key(key(KeyCode::Char('T')));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down));
        let chosen = app.theme.name.clone();
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Action::SaveTheme {
                name: chosen.clone(),
                transparent: None
            })
        );
        assert_eq!(app.theme.name, chosen);

        // reopening starts on the current theme; cancel keeps it
        app.handle_key(key(KeyCode::Char('T')));
        assert_eq!(app.theme_picker.as_ref().unwrap().cursor, 2);
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.theme.name, chosen);
    }

    #[test]
    fn theme_picker_cursor_is_bounded() {
        let mut app = themed_app();
        app.handle_key(key(KeyCode::Char('T')));
        for _ in 0..50 {
            app.handle_key(key(KeyCode::Down));
        }
        assert_eq!(
            app.theme_picker.as_ref().unwrap().cursor,
            app.themes.len() - 1
        );
        for _ in 0..50 {
            app.handle_key(key(KeyCode::Up));
        }
        assert_eq!(app.theme_picker.as_ref().unwrap().cursor, 0);
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn limits_dialog_parses_and_applies() {
        let mut app = App::new(String::new());
        app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(app.mode, Mode::Limits);
        type_str(&mut app, "2MiB/s");
        app.handle_key(key(KeyCode::Tab));
        type_str(&mut app, "500KiB/s");
        let action = app.handle_key(key(KeyCode::Enter));
        let (down, up) = (
            "2MiB/s".parse::<Rate>().unwrap(),
            "500KiB/s".parse::<Rate>().unwrap(),
        );
        assert_eq!(
            action,
            Some(Action::SetLimits {
                download: down,
                upload: up
            })
        );
        assert_eq!(app.limits, (down, up));
        assert_eq!(app.mode, Mode::Normal);

        // reopening prefills the current values; clearing a field = unlimited
        app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(
            app.limits_dialog.as_ref().unwrap().download.text(),
            "2MiB/s"
        );
        app.handle_key(key(KeyCode::Char('u')));
        let mut k = key(KeyCode::Char('u'));
        k.modifiers = KeyModifiers::CONTROL;
        app.handle_key(k);
        let action = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            action,
            Some(Action::SetLimits {
                download: Rate::default(),
                upload: up
            })
        );
    }

    #[test]
    fn limits_dialog_rejects_bad_input_and_stays_open() {
        let mut app = App::new(String::new());
        app.handle_key(key(KeyCode::Char('l')));
        type_str(&mut app, "500");
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(app.mode, Mode::Limits);
        let err = app.limits_dialog.as_ref().unwrap().error.clone().unwrap();
        assert!(
            err.starts_with("Download:") && err.contains("unit"),
            "{err}"
        );
        assert_eq!(app.limits, (Rate::default(), Rate::default()), "unchanged");

        // typing clears the error; fix it and apply
        app.handle_key(key(KeyCode::Char('K')));
        assert!(app.limits_dialog.as_ref().unwrap().error.is_none());
        let action = app.handle_key(key(KeyCode::Enter));
        assert!(matches!(action, Some(Action::SetLimits { .. })));

        // a bad upload value focuses the upload field
        app.handle_key(key(KeyCode::Char('l')));
        app.handle_key(key(KeyCode::Tab));
        type_str(&mut app, "5Mbps");
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        let d = app.limits_dialog.as_ref().unwrap();
        assert_eq!(d.focus, 1);
        assert!(d.error.as_ref().unwrap().starts_with("Upload:"));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn paste_goes_to_the_focused_limit_field() {
        let mut app = App::new(String::new());
        app.handle_key(key(KeyCode::Char('l')));
        app.handle_key(key(KeyCode::Tab));
        app.handle_paste(" 3MiB/s\n");
        let d = app.limits_dialog.as_ref().unwrap();
        assert_eq!(d.download.text(), "");
        assert_eq!(d.upload.text(), "3MiB/s");
    }

    #[test]
    fn history_is_bounded() {
        let mut app = App::new(String::new());
        for _ in 0..(HISTORY_LEN + 20) {
            app.update_torrents(vec![view(1)]);
        }
        assert_eq!(app.down_history.len(), HISTORY_LEN);
    }

    fn tview(id: usize) -> TorrentView {
        TorrentView {
            info_hash: format!("h{id}"),
            ..view(id)
        }
    }

    /// Three torrents; t1 = "movies 4k", t2 = "movies", t3 untagged.
    fn tagged_app() -> App {
        let mut app = App::new(String::new());
        app.update_torrents(vec![tview(1), tview(2), tview(3)]);
        let mut tags = Tags::new();
        tags.insert("h1".into(), vec!["4k".into(), "movies".into()]);
        tags.insert("h2".into(), vec!["movies".into()]);
        app.set_tags(tags);
        app
    }

    fn visible_ids(app: &App) -> Vec<usize> {
        app.visible().map(|t| t.id).collect()
    }

    #[test]
    fn tag_editor_adds_new_tags_normalised() {
        let mut app = App::new(String::new());
        app.update_torrents(vec![tview(1)]);
        app.handle_key(key(KeyCode::Char('t')));
        assert_eq!(app.mode, Mode::Tags);
        app.handle_paste("Movies, #4K  movies\n");
        // First Enter adds what was typed, the second saves.
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(app.mode, Mode::Tags);
        let a = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            a,
            Some(Action::SetTags {
                info_hash: "h1".into(),
                tags: vec!["4k".into(), "movies".into()],
            })
        );
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.tags["h1"], ["4k", "movies"]);
    }

    #[test]
    fn tag_editor_lists_known_tags_checked_for_this_torrent() {
        let mut app = tagged_app();
        // An action's tag is offered even though no torrent has it.
        app.hooks = vec![HookSpec {
            name: None,
            run: vec!["x".into()],
            tags: vec!["sport".into()],
            timeout_secs: 5,
        }];
        app.handle_key(key(KeyCode::Char('t')));
        let rows: Vec<_> = app
            .tag_editor
            .as_ref()
            .unwrap()
            .list
            .rows
            .iter()
            .map(|r| (r.name.as_str(), r.checked, r.count))
            .collect();
        assert_eq!(
            rows,
            [("4k", true, 1), ("movies", true, 2), ("sport", false, 0)]
        );
    }

    #[test]
    fn unchecking_every_tag_clears_them() {
        let mut app = tagged_app();
        app.handle_key(key(KeyCode::Char('t')));
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Char(' ')));
        let a = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            a,
            Some(Action::SetTags {
                info_hash: "h1".into(),
                tags: vec![],
            })
        );
        assert!(!app.tags.contains_key("h1"));
    }

    #[test]
    fn checking_an_existing_tag_adds_it_to_the_torrent() {
        let mut app = tagged_app();
        app.handle_key(key(KeyCode::Down)); // torrent 2: movies
        app.handle_key(key(KeyCode::Char('t')));
        app.handle_key(key(KeyCode::Char(' '))); // 4k on
        let a = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            a,
            Some(Action::SetTags {
                info_hash: "h2".into(),
                tags: vec!["4k".into(), "movies".into()],
            })
        );
    }

    #[test]
    fn tag_editor_rejects_bad_tags_and_stays_open() {
        let mut app = tagged_app();
        app.handle_key(key(KeyCode::Char('t')));
        type_str(&mut app, "bad!");
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(app.mode, Mode::Tags);
        assert!(
            app.tag_editor
                .as_ref()
                .unwrap()
                .list
                .error
                .as_ref()
                .unwrap()
                .contains("'!'")
        );
        // Nothing was applied, and editing clears the error.
        assert_eq!(app.tags["h1"], ["4k", "movies"]);
        app.handle_key(key(KeyCode::Backspace));
        assert!(app.tag_editor.as_ref().unwrap().list.error.is_none());
        // Esc discards without touching the tags.
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.tags["h1"], ["4k", "movies"]);
    }

    #[test]
    fn typing_command_letters_in_the_tag_editor_does_not_trigger_them() {
        let mut app = tagged_app();
        app.handle_key(key(KeyCode::Char('t')));
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), None);
        type_str(&mut app, "dT/");
        assert_eq!(app.mode, Mode::Tags);
    }

    #[test]
    fn tag_editor_needs_a_torrent() {
        let mut app = App::new(String::new());
        app.handle_key(key(KeyCode::Char('t')));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn filter_picker_lists_counts_and_applies() {
        let mut app = tagged_app();
        app.handle_key(key(KeyCode::Char('/')));
        assert_eq!(app.mode, Mode::Filter);
        let entries = app.filter_picker.as_ref().unwrap().entries.clone();
        assert_eq!(
            entries,
            [
                (Filter::All, 3),
                (Filter::Untagged, 1),
                (Filter::Tag("4k".into()), 1),
                (Filter::Tag("movies".into()), 2),
            ]
        );
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Char('j')));
        }
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.filter, Filter::Tag("movies".into()));
        assert_eq!(visible_ids(&app), [1, 2]);

        // Reopening starts on the active filter; Untagged works too.
        app.handle_key(key(KeyCode::Char('/')));
        assert_eq!(app.filter_picker.as_ref().unwrap().cursor, 3);
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.filter, Filter::Untagged);
        assert_eq!(visible_ids(&app), [3]);

        // Esc in the list clears the filter.
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.filter, Filter::All);
        assert_eq!(visible_ids(&app), [1, 2, 3]);
    }

    #[test]
    fn filter_picker_cancel_keeps_filter_and_needs_tags() {
        let mut app = tagged_app();
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.filter, Filter::All);
        assert_eq!(app.mode, Mode::Normal);

        let mut empty = App::new(String::new());
        empty.update_torrents(vec![tview(1)]);
        empty.handle_key(key(KeyCode::Char('/')));
        assert_eq!(empty.mode, Mode::Normal);
        assert!(empty.status().unwrap().0.contains("No tags yet"));
    }

    #[test]
    fn selection_and_actions_follow_the_filtered_list() {
        let mut app = tagged_app();
        app.filter = Filter::Tag("movies".into());
        app.refresh_view();
        app.select(usize::MAX);
        assert_eq!(app.current().unwrap().id, 2);
        // Pause acts on the visible torrent, not on index 1 of all torrents.
        assert_eq!(
            app.handle_key(key(KeyCode::Char('p'))),
            Some(Action::TogglePause(2))
        );
        // Selection sticks to the torrent across refreshes.
        app.update_torrents(vec![tview(1), tview(2), tview(3)]);
        assert_eq!(app.current().unwrap().id, 2);
    }

    #[test]
    fn retagging_updates_the_filtered_list() {
        let mut app = tagged_app();
        app.filter = Filter::Tag("4k".into());
        app.refresh_view();
        assert_eq!(visible_ids(&app), [1]);
        // Unticking the only "4k" tag empties the filter, which resets itself.
        app.handle_key(key(KeyCode::Char('t')));
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.filter, Filter::All);
        assert_eq!(visible_ids(&app), [1, 2, 3]);
        assert!(app.status().unwrap().0.contains("Filter cleared"));
    }

    fn state(torrents: Vec<TorrentView>) -> State {
        State {
            torrents,
            hooks: vec![],
            tags: Tags::new(),
            download_limit: None,
            upload_limit: None,
            pickers: vec![],
            adding: 0,
            actions_running: 0,
        }
    }

    fn picker_info(token: u64) -> PickerInfo {
        PickerInfo {
            token,
            name: format!("p{token}"),
            files: entries(3),
        }
    }

    #[test]
    fn state_replaces_torrents_tags_limits_and_counters() {
        let mut app = App::new(String::new());
        let mut st = state(vec![tview(1), tview(2)]);
        st.tags.insert("h2".into(), vec!["x".into()]);
        st.download_limit = Some(2048);
        st.adding = 2;
        st.actions_running = 1;
        app.apply_state(st);
        assert_eq!(app.torrents.len(), 2);
        assert_eq!(app.tags["h2"], ["x"]);
        assert_eq!(app.limits.0.to_text(), "2KiB/s");
        assert_eq!(app.limits.1.to_text(), "unlimited");
        assert_eq!((app.pending_adds, app.actions_running), (2, 1));
        // A later state fully replaces them.
        app.apply_state(state(vec![tview(1)]));
        assert!(app.tags.is_empty() && app.limits.0.bps().is_none());
        assert_eq!((app.pending_adds, app.actions_running), (0, 0));
    }

    #[test]
    fn state_does_not_clobber_a_setting_change_in_flight() {
        let mut app = tagged_app();
        app.limits.0 = "1MiB/s".parse().unwrap();
        app.settings_in_flight = 1;
        // The daemon hasn't seen our change yet, so its state is stale.
        app.apply_state(state(vec![tview(1), tview(2), tview(3)]));
        assert_eq!(app.tags["h1"], ["4k", "movies"]);
        assert_eq!(app.limits.0.to_text(), "1MiB/s");
        // Once acknowledged, the daemon's values win again.
        app.settings_in_flight = 0;
        app.apply_state(state(vec![tview(1), tview(2), tview(3)]));
        assert!(app.tags.is_empty() && app.limits.0.bps().is_none());
    }

    #[test]
    fn filter_uses_the_tags_from_the_same_state() {
        let mut app = App::new(String::new());
        let mut st = state(vec![tview(1), tview(2)]);
        st.tags.insert("h2".into(), vec!["x".into()]);
        app.filter = Filter::Tag("x".into());
        app.apply_state(st);
        assert_eq!(app.filter, Filter::Tag("x".into()));
        assert_eq!(visible_ids(&app), [2]);
    }

    #[test]
    fn daemon_pickers_open_once_and_vanish_when_answered_elsewhere() {
        let mut app = App::new(String::new());
        let with = |tokens: &[u64]| {
            let mut st = state(vec![]);
            st.pickers = tokens.iter().map(|t| picker_info(*t)).collect();
            st
        };
        app.apply_state(with(&[1, 2]));
        assert_eq!(app.mode, Mode::Files);
        assert!(matches!(
            app.picker.as_ref().unwrap().target,
            PickerTarget::New { token: 1, .. }
        ));
        // The same pickers arriving again don't reopen or duplicate.
        app.apply_state(with(&[1, 2]));
        assert!(app.picker_queue.len() == 1 && app.picker.is_some());

        // Token 2 is answered in another window: dropped from the queue.
        app.apply_state(with(&[1]));
        assert!(app.picker_queue.is_empty());
        // Token 1 too: the open dialog closes with an explanation.
        app.apply_state(with(&[]));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.picker.is_none());
        assert!(app.status().unwrap().0.contains("another window"));
    }

    #[test]
    fn answering_a_picker_locally_does_not_reopen_it() {
        let mut app = App::new(String::new());
        let mut st = state(vec![]);
        st.pickers = vec![picker_info(5)];
        app.apply_state(st.clone());
        let a = app.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            a,
            Some(Action::ApplyFiles {
                target: PickerTarget::New { token: 5, .. },
                ..
            })
        ));
        // The daemon still lists it until it processes our answer.
        app.apply_state(st);
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.picker.is_none());
        // Once gone, the quiet cleanup shows no "another window" message.
        app.clear_status();
        app.apply_state(state(vec![]));
        assert!(app.status().is_none());
    }

    #[test]
    fn cancelling_a_daemon_picker_releases_it_for_everyone() {
        let mut app = App::new(String::new());
        let mut st = state(vec![]);
        st.pickers = vec![picker_info(5)];
        app.apply_state(st);
        assert_eq!(
            app.handle_key(key(KeyCode::Esc)),
            Some(Action::CancelPicker(5))
        );
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn capital_q_always_asks_before_stopping_the_daemon() {
        let mut app = App::new(String::new());
        app.update_torrents(vec![TorrentView {
            status: Status::Seeding,
            ..tview(1)
        }]);
        assert!(!app.is_busy());
        assert_eq!(app.handle_key(key(KeyCode::Char('Q'))), None);
        assert_eq!(app.mode, Mode::ConfirmStop);
        assert_eq!(app.handle_key(key(KeyCode::Char('n'))), None);
        assert_eq!(app.mode, Mode::Normal);

        // Downloading: still asks, and only `y` confirms.
        app.update_torrents(vec![tview(1)]);
        assert!(app.is_busy());
        assert_eq!(app.handle_key(key(KeyCode::Char('Q'))), None);
        assert_eq!(app.mode, Mode::ConfirmStop);
        for k in [KeyCode::Enter, KeyCode::Char('x'), KeyCode::Char('Q')] {
            assert_eq!(app.handle_key(key(k)), None);
            assert_eq!(app.mode, Mode::ConfirmStop);
        }
        assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
        assert_eq!(app.mode, Mode::Normal);
        app.handle_key(key(KeyCode::Char('Q')));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            Some(Action::StopDaemon)
        );

        // Lowercase q only detaches.
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Some(Action::Quit));
    }

    #[test]
    fn pending_adds_and_running_actions_count_as_busy() {
        let mut app = App::new(String::new());
        let mut st = state(vec![]);
        st.adding = 1;
        app.apply_state(st);
        app.handle_key(key(KeyCode::Char('Q')));
        assert_eq!(app.mode, Mode::ConfirmStop);
    }

    #[test]
    fn totals_ignore_the_filter() {
        let mut app = tagged_app();
        app.filter = Filter::Untagged;
        app.refresh_view();
        assert_eq!(app.total_down(), 30);
    }

    #[test]
    fn x_runs_actions_only_when_configured_and_finished() {
        let mut app = App::new(String::new());
        app.update_torrents(vec![tview(1)]);
        // Not configured.
        assert_eq!(app.handle_key(key(KeyCode::Char('x'))), None);
        assert!(app.status().unwrap().0.contains("No on_complete actions"));

        // Configured, but still downloading.
        app.has_actions = true;
        assert_eq!(app.handle_key(key(KeyCode::Char('x'))), None);
        assert!(app.status().unwrap().0.contains("still downloading"));

        // Finished (seeding, or paused with everything downloaded).
        let mut done = tview(1);
        done.status = Status::Seeding;
        app.update_torrents(vec![done.clone()]);
        assert_eq!(
            app.handle_key(key(KeyCode::Char('x'))),
            Some(Action::RunActions(1))
        );
        done.status = Status::Paused;
        done.total_bytes = 100;
        done.progress_bytes = 100;
        app.update_torrents(vec![done]);
        assert_eq!(
            app.handle_key(key(KeyCode::Char('x'))),
            Some(Action::RunActions(1))
        );

        // Nothing selected: no action, no panic.
        let mut empty = App::new(String::new());
        empty.has_actions = true;
        assert_eq!(empty.handle_key(key(KeyCode::Char('x'))), None);
    }

    fn hook(name: &str, tags: &[&str]) -> HookSpec {
        HookSpec {
            name: Some(name.into()),
            run: vec!["prog".into(), "{path}".into()],
            tags: tags.iter().map(|t| t.to_string()).collect(),
            timeout_secs: 60,
        }
    }

    #[test]
    fn state_carries_the_hooks_and_enables_the_x_key() {
        let mut app = App::new(String::new());
        assert!(!app.has_actions);
        let mut st = state(vec![]);
        st.hooks = vec![hook("a", &[])];
        app.apply_state(st);
        assert!(app.has_actions);
        assert_eq!(app.hooks.len(), 1);
        app.apply_state(state(vec![]));
        assert!(!app.has_actions);
    }

    #[test]
    fn hook_manager_adds_an_action_from_the_form() {
        let mut app = tagged_app();
        app.handle_key(key(KeyCode::Char('A')));
        assert_eq!(app.mode, Mode::Hooks);
        app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(app.mode, Mode::HookEdit);
        type_str(&mut app, "sort");
        app.handle_key(key(KeyCode::Tab));
        type_str(&mut app, "prog {path}");
        // Pick a tag from the list (4k is first), then save.
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Char(' ')));
        let a = app.handle_key(key(KeyCode::Enter));
        let expected = HookSpec {
            name: Some("sort".into()),
            run: vec!["prog".into(), "{path}".into()],
            tags: vec!["4k".into()],
            timeout_secs: 600,
        };
        assert_eq!(a, Some(Action::SetHooks(vec![expected.clone()])));
        assert_eq!(app.mode, Mode::Hooks);
        assert_eq!(app.hooks, [expected]);
        assert!(app.has_actions);
    }

    #[test]
    fn hook_form_errors_keep_it_open_and_esc_goes_back() {
        let mut app = tagged_app();
        app.handle_key(key(KeyCode::Char('A')));
        app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(app.mode, Mode::HookEdit);
        assert!(
            app.hook_editor
                .as_ref()
                .unwrap()
                .error
                .as_ref()
                .unwrap()
                .contains("command is required")
        );
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Hooks);
        assert!(app.hooks.is_empty());
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn hook_manager_edits_and_deletes_with_confirmation() {
        let mut app = App::new(String::new());
        app.hooks = vec![hook("a", &[]), hook("b", &["x"])];
        app.handle_key(key(KeyCode::Char('A')));
        app.handle_key(key(KeyCode::Down));
        // Edit: the form is prefilled, and saving replaces in place.
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.mode, Mode::HookEdit);
        assert_eq!(app.hook_editor.as_ref().unwrap().name.text(), "b");
        type_str(&mut app, "2");
        let a = app.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(&a, Some(Action::SetHooks(h)) if h.len() == 2 && h[1].name.as_deref() == Some("b2"))
        );
        assert_eq!(app.hooks[0].name.as_deref(), Some("a"));

        // Delete asks first; anything but y cancels.
        app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(app.handle_key(key(KeyCode::Char('x'))), None);
        assert_eq!(app.hooks.len(), 2);
        app.handle_key(key(KeyCode::Char('d')));
        app.handle_key(key(KeyCode::Char('n')));
        assert_eq!(app.hooks.len(), 2);
        app.handle_key(key(KeyCode::Char('d')));
        let a = app.handle_key(key(KeyCode::Char('y')));
        assert!(
            matches!(&a, Some(Action::SetHooks(h)) if h.len() == 1 && h[0].name.as_deref() == Some("a"))
        );
        assert_eq!(app.hook_manager.as_ref().unwrap().cursor, 0);
        app.handle_key(key(KeyCode::Char('d')));
        let a = app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(a, Some(Action::SetHooks(vec![])));
        assert!(!app.has_actions);
    }

    #[test]
    fn deleting_a_tag_removes_it_everywhere() {
        let mut app = tagged_app();
        app.hooks = vec![hook("both", &["movies", "tv"]), hook("all", &[])];
        app.filter = Filter::Tag("4k".into());
        app.refresh_view();
        app.handle_key(key(KeyCode::Char('t')));
        // Rows: 4k (cursor), movies, tv. Ask to delete 4k, then confirm.
        app.handle_key(key(KeyCode::Delete));
        assert_eq!(app.handle_key(key(KeyCode::Char('x'))), None, "x backs out");
        assert_eq!(app.tags["h1"], ["4k", "movies"]);
        app.handle_key(key(KeyCode::Delete));
        let a = app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(a, Some(Action::DeleteTag("4k".into())));
        assert_eq!(app.tags["h1"], ["movies"]);
        assert_eq!(app.mode, Mode::Tags, "editor stays open");
        let names: Vec<_> = app
            .tag_editor
            .as_ref()
            .unwrap()
            .list
            .rows
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(names, ["movies", "tv"]);
        assert_eq!(
            app.filter,
            Filter::All,
            "a filter on the deleted tag resets"
        );

        // Tags drop out of actions that have others; none are left bare.
        app.handle_key(key(KeyCode::Down)); // tv
        app.handle_key(key(KeyCode::Delete));
        let a = app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(a, Some(Action::DeleteTag("tv".into())));
        assert_eq!(app.hooks[0].tags, ["movies"]);
    }

    #[test]
    fn a_tag_that_an_action_depends_on_cannot_be_deleted() {
        let mut app = tagged_app();
        app.hooks = vec![hook("only movies", &["movies"])];
        app.handle_key(key(KeyCode::Char('t')));
        app.handle_key(key(KeyCode::Down)); // movies
        app.handle_key(key(KeyCode::Delete));
        assert_eq!(app.handle_key(key(KeyCode::Char('y'))), None);
        let err = app.tag_editor.as_ref().unwrap().list.error.clone().unwrap();
        assert!(
            err.contains("only movies") && err.contains("only runs for #movies"),
            "{err}"
        );
        assert_eq!(app.tags["h2"], ["movies"]);
        assert_eq!(app.tag_editor.as_ref().unwrap().list.rows.len(), 2);
    }

    #[test]
    fn esc_cancels_a_pending_delete_before_closing_the_editor() {
        let mut app = tagged_app();
        app.handle_key(key(KeyCode::Char('t')));
        app.handle_key(key(KeyCode::Delete));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Tags);
        assert!(
            app.tag_editor
                .as_ref()
                .unwrap()
                .list
                .confirm_delete
                .is_none()
        );
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn capital_e_asks_to_edit_the_config_only_from_the_list() {
        let mut app = App::new(String::new());
        assert_eq!(
            app.handle_key(key(KeyCode::Char('E'))),
            Some(Action::EditConfig)
        );
        // Typing a capital E into a dialog is just text.
        app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(app.handle_key(key(KeyCode::Char('E'))), None);
        assert_eq!(app.input.text(), "E");
    }

    #[test]
    fn theme_picker_toggles_background_with_b_and_saves_only_a_change() {
        let mut app = App::new(String::new());
        app.set_themes(theme::builtin_themes(), "dracula");
        assert!(app.transparent);
        assert_eq!(app.theme.bg, ratatui::style::Color::Reset);
        // B no longer does anything outside the picker.
        assert_eq!(app.handle_key(key(KeyCode::Char('B'))), None);
        assert!(app.transparent);

        // Previewed live; cancelling puts the old setting back.
        app.handle_key(key(KeyCode::Char('T')));
        app.handle_key(key(KeyCode::Char('b')));
        assert!(!app.transparent);
        assert_ne!(app.theme.bg, ratatui::style::Color::Reset);
        app.handle_key(key(KeyCode::Esc));
        assert!(app.transparent);
        assert_eq!(app.theme.bg, ratatui::style::Color::Reset);

        // Saving reports the change, and the previewed theme keeps it.
        app.handle_key(key(KeyCode::Char('T')));
        app.handle_key(key(KeyCode::Char('b')));
        app.handle_key(key(KeyCode::Down));
        assert_ne!(app.theme.bg, ratatui::style::Color::Reset, "still opaque");
        let a = app.handle_key(key(KeyCode::Enter));
        let name = app.theme.name.clone();
        assert_eq!(
            a,
            Some(Action::SaveTheme {
                name,
                transparent: Some(false)
            })
        );
        assert!(!app.transparent);

        // Reopening and just choosing a theme leaves the setting alone.
        app.handle_key(key(KeyCode::Char('T')));
        let a = app.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            a,
            Some(Action::SaveTheme {
                transparent: None,
                ..
            })
        ));
    }

    fn add_with(app: &mut App, link: &str) {
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_paste(link);
    }

    #[test]
    fn add_dialog_sends_the_ticked_tags() {
        let mut app = tagged_app();
        add_with(&mut app, "magnet:?xt=urn:btih:abc");
        // Tab to the list (4k, movies), tick movies, Enter on a tag saves.
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Char(' ')));
        let a = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            a,
            Some(Action::Add {
                input: "magnet:?xt=urn:btih:abc".into(),
                tags: vec!["movies".into()],
            })
        );
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.add_tags.is_none());
    }

    #[test]
    fn add_dialog_creates_a_new_tag_and_enter_in_the_link_field_submits() {
        let mut app = tagged_app();
        add_with(&mut app, "magnet:?xt=urn:btih:abc");
        app.handle_key(key(KeyCode::Down)); // into the tag list
        type_str(&mut app, "Fresh");
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            None,
            "Enter adds the tag"
        );
        assert_eq!(app.mode, Mode::Add);
        app.handle_key(key(KeyCode::Tab)); // back to the link
        let a = app.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            &a,
            Some(Action::Add { tags, .. }) if tags == &["fresh".to_string()]
        ));

        // No tags chosen: an ordinary add.
        let mut app = tagged_app();
        add_with(&mut app, "magnet:?xt=urn:btih:def");
        assert!(matches!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Action::Add { tags, .. }) if tags.is_empty()
        ));
    }

    #[test]
    fn typing_in_the_link_field_does_not_touch_the_tag_list() {
        let mut app = tagged_app();
        add_with(&mut app, "");
        type_str(&mut app, "dTq ");
        assert_eq!(app.input.text(), "dTq ");
        assert!(!app.add_focus_tags);
        assert_eq!(app.mode, Mode::Add);
    }

    #[test]
    fn new_torrents_from_elsewhere_prompt_for_tags_once() {
        let mut app = App::new(String::new());
        app.ask_tags = true;
        app.update_torrents(vec![tview(1)]);
        app.open_next_tag_prompt();
        assert_eq!(
            app.mode,
            Mode::Normal,
            "what was already there is not prompted"
        );

        app.update_torrents(vec![tview(1), tview(2)]);
        app.open_next_tag_prompt();
        assert_eq!(app.mode, Mode::Tags);
        let editor = app.tag_editor.as_ref().unwrap();
        assert!(editor.prompt && editor.name == "t2");

        // Tag it like any other.
        app.handle_paste("anime");
        app.handle_key(key(KeyCode::Enter));
        let a = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            a,
            Some(Action::SetTags {
                info_hash: "h2".into(),
                tags: vec!["anime".into()],
            })
        );
        // Not asked again, and Esc skips without tagging.
        app.update_torrents(vec![tview(1), tview(2), tview(3)]);
        app.open_next_tag_prompt();
        assert_eq!(app.mode, Mode::Tags);
        app.handle_key(key(KeyCode::Esc));
        app.open_next_tag_prompt();
        assert_eq!(app.mode, Mode::Normal);
        assert!(!app.tags.contains_key("h3"));
    }

    #[test]
    fn tag_prompts_wait_for_other_dialogs_and_skip_tagged_or_gone_torrents() {
        let mut app = App::new(String::new());
        app.ask_tags = true;
        app.update_torrents(vec![tview(1)]);
        app.update_torrents(vec![tview(1), tview(2), tview(3)]);
        app.handle_key(key(KeyCode::Char('l')));
        app.open_next_tag_prompt();
        assert_eq!(app.mode, Mode::Limits, "must not steal focus");
        app.handle_key(key(KeyCode::Esc));
        // Torrent 2 got tagged elsewhere meanwhile, so 3 is offered.
        let mut tags = Tags::new();
        tags.insert("h2".into(), vec!["x".into()]);
        app.set_tags(tags);
        app.open_next_tag_prompt();
        assert_eq!(app.tag_editor.as_ref().unwrap().name, "t3");
        app.handle_key(key(KeyCode::Esc));
        // A torrent that vanished is dropped quietly.
        app.update_torrents(vec![tview(1), tview(4)]);
        app.update_torrents(vec![tview(1)]);
        app.open_next_tag_prompt();
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn torrents_added_here_or_with_the_setting_off_are_not_prompted() {
        let mut app = App::new(String::new());
        app.ask_tags = true;
        app.update_torrents(vec![tview(1)]);
        add_with(&mut app, "magnet:?xt=urn:btih:abc");
        app.handle_key(key(KeyCode::Enter));
        app.update_torrents(vec![tview(1), tview(2)]);
        app.open_next_tag_prompt();
        assert_eq!(app.mode, Mode::Normal, "its tags were chosen in the dialog");
        // The next arrival, from a browser, is.
        app.update_torrents(vec![tview(1), tview(2), tview(3)]);
        app.open_next_tag_prompt();
        assert_eq!(app.mode, Mode::Tags);

        let mut off = App::new(String::new());
        off.update_torrents(vec![tview(1)]);
        off.update_torrents(vec![tview(1), tview(2)]);
        off.open_next_tag_prompt();
        assert_eq!(off.mode, Mode::Normal);
    }
}
