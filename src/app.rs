//! Application state and input handling. No rendering and no I/O here, so it
//! can be unit-tested without a terminal or a torrent session.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::engine::TorrentView;

const HISTORY_LEN: usize = 120;
/// How long a status message stays visible.
const STATUS_TTL: Duration = Duration::from_secs(6);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Add,
    ConfirmRemove { id: usize, name: String },
    Help,
}

/// Side effects requested by input handling; executed by the event loop.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Add(String),
    TogglePause(usize),
    Remove { id: usize, delete_files: bool },
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
    pub fn len(&self) -> usize {
        self.chars.len()
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

pub struct App {
    pub mode: Mode,
    pub input: Input,
    pub torrents: Vec<TorrentView>,
    pub selected: usize,
    selected_id: Option<usize>,
    status: Option<(String, bool, Instant)>,
    pub pending_adds: usize,
    pub down_history: VecDeque<u64>,
    pub download_dir: String,
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
            down_history: VecDeque::with_capacity(HISTORY_LEN),
            download_dir,
        }
    }

    pub fn set_status(&mut self, msg: impl Into<String>, is_error: bool) {
        self.status = Some((msg.into(), is_error, Instant::now()));
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

    pub fn current(&self) -> Option<&TorrentView> {
        self.torrents.get(self.selected)
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
        let by_id = self
            .selected_id
            .and_then(|id| self.torrents.iter().position(|t| t.id == id));
        self.selected = match by_id {
            Some(i) => i,
            None => self.selected.min(self.torrents.len().saturating_sub(1)),
        };
        self.selected_id = self.current().map(|t| t.id);

        if self.down_history.len() == HISTORY_LEN {
            self.down_history.pop_front();
        }
        let total = self.total_down();
        self.down_history.push_back(total);
    }

    fn select(&mut self, index: usize) {
        if self.torrents.is_empty() {
            return;
        }
        self.selected = index.min(self.torrents.len() - 1);
        self.selected_id = self.current().map(|t| t.id);
    }

    pub fn handle_paste(&mut self, text: &str) {
        if self.mode == Mode::Add {
            self.input.insert_str(text.trim());
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(Action::Quit);
        }
        match self.mode.clone() {
            Mode::Normal => self.key_normal(key),
            Mode::Add => self.key_add(key),
            Mode::ConfirmRemove { id, .. } => self.key_confirm(key, id),
            Mode::Help => {
                // Any key closes help.
                self.mode = Mode::Normal;
                None
            }
        }
    }

    fn key_normal(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Char('q') => return Some(Action::Quit),
            KeyCode::Char('a') => {
                self.input.clear();
                self.mode = Mode::Add;
            }
            KeyCode::Char('?') | KeyCode::F(1) => self.mode = Mode::Help,
            KeyCode::Down | KeyCode::Char('j') => self.select(self.selected + 1),
            KeyCode::Up | KeyCode::Char('k') => self.select(self.selected.saturating_sub(1)),
            KeyCode::Home | KeyCode::Char('g') => self.select(0),
            KeyCode::End | KeyCode::Char('G') => self.select(usize::MAX),
            KeyCode::Char('p') | KeyCode::Char(' ') => {
                return self.current().map(|t| Action::TogglePause(t.id));
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(t) = self.current() {
                    self.mode = Mode::ConfirmRemove {
                        id: t.id,
                        name: t.name.clone(),
                    };
                }
            }
            _ => {}
        }
        None
    }

    fn key_add(&mut self, key: KeyEvent) -> Option<Action> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                let text = self.input.text();
                self.mode = Mode::Normal;
                self.input.clear();
                if !text.trim().is_empty() {
                    return Some(Action::Add(text));
                }
            }
            KeyCode::Char('u') if ctrl => self.input.clear(),
            KeyCode::Char('a') if ctrl => self.input.cursor = 0,
            KeyCode::Char('e') if ctrl => self.input.cursor = self.input.len(),
            KeyCode::Char(c) if !ctrl => {
                let mut buf = [0u8; 4];
                self.input.insert_str(c.encode_utf8(&mut buf));
            }
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.cursor = self.input.cursor.saturating_sub(1),
            KeyCode::Right => self.input.cursor = (self.input.cursor + 1).min(self.input.len()),
            KeyCode::Home => self.input.cursor = 0,
            KeyCode::End => self.input.cursor = self.input.len(),
            _ => {}
        }
        None
    }

    fn key_confirm(&mut self, key: KeyEvent, id: usize) -> Option<Action> {
        let action = match key.code {
            KeyCode::Char('y') | KeyCode::Enter => Some(Action::Remove {
                id,
                delete_files: false,
            }),
            KeyCode::Char('D') => Some(Action::Remove {
                id,
                delete_files: true,
            }),
            KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => None,
            _ => return None, // ignore other keys, stay in the dialog
        };
        self.mode = Mode::Normal;
        action
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
        assert_eq!(a, Some(Action::Add("magnet:?xt=urn:btih:abc".into())));
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
    fn remove_requires_confirmation() {
        let mut app = App::new(String::new());
        app.update_torrents(vec![view(7)]);
        assert_eq!(app.handle_key(key(KeyCode::Char('d'))), None);
        assert!(matches!(app.mode, Mode::ConfirmRemove { id: 7, .. }));
        assert_eq!(app.handle_key(key(KeyCode::Char('x'))), None);
        assert!(matches!(app.mode, Mode::ConfirmRemove { .. }));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('D'))),
            Some(Action::Remove {
                id: 7,
                delete_files: true
            })
        );
        assert_eq!(app.mode, Mode::Normal);

        app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(app.handle_key(key(KeyCode::Char('n'))), None);
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

    #[test]
    fn history_is_bounded() {
        let mut app = App::new(String::new());
        for _ in 0..(HISTORY_LEN + 20) {
            app.update_torrents(vec![view(1)]);
        }
        assert_eq!(app.down_history.len(), HISTORY_LEN);
    }
}
