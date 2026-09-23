//! Application state and key handling.

use crate::config;
use crate::procs;
use crate::tunnel::{ProcEvent, Status, Tunnel, TunnelConfig};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

const STATUS_TTL: Duration = Duration::from_secs(4);

/// Simple single-line text input with a cursor (in chars).
#[derive(Default, Clone)]
pub struct Input {
    pub text: String,
    pub cursor: usize,
}

impl Input {
    pub fn with(text: &str) -> Self {
        Self {
            text: text.to_string(),
            cursor: text.chars().count(),
        }
    }
    fn byte_idx(&self, ci: usize) -> usize {
        self.text
            .char_indices()
            .nth(ci)
            .map(|(b, _)| b)
            .unwrap_or(self.text.len())
    }
    pub fn insert(&mut self, c: char) {
        let b = self.byte_idx(self.cursor);
        self.text.insert(b, c);
        self.cursor += 1;
    }
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            let b = self.byte_idx(self.cursor);
            self.text.remove(b);
        }
    }
    pub fn delete(&mut self) {
        if self.cursor < self.text.chars().count() {
            let b = self.byte_idx(self.cursor);
            self.text.remove(b);
        }
    }
    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }
    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.text.chars().count());
    }
    pub fn home(&mut self) {
        self.cursor = 0;
    }
    pub fn end(&mut self) {
        self.cursor = self.text.chars().count();
    }
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }
}

pub const FORM_FIELDS: usize = 5;

pub struct Form {
    pub editing: Option<u64>,
    pub name: Input,
    pub source: Input,
    pub destination: Input,
    pub options: Input,
    pub autostart: bool,
    pub focus: usize,
    pub error: Option<String>,
}

impl Form {
    fn new() -> Self {
        Self {
            editing: None,
            name: Input::default(),
            source: Input::default(),
            destination: Input::default(),
            options: Input::default(),
            autostart: false,
            focus: 0,
            error: None,
        }
    }
    fn from_config(id: u64, c: &TunnelConfig) -> Self {
        Self {
            editing: Some(id),
            name: Input::with(&c.name),
            source: Input::with(&c.source),
            destination: Input::with(&c.destination),
            options: Input::with(&c.options),
            autostart: c.autostart,
            focus: 0,
            error: None,
        }
    }
    fn input_mut(&mut self) -> Option<&mut Input> {
        match self.focus {
            0 => Some(&mut self.name),
            1 => Some(&mut self.source),
            2 => Some(&mut self.destination),
            3 => Some(&mut self.options),
            _ => None,
        }
    }
    fn to_config(&self) -> Result<TunnelConfig, String> {
        let name = self.name.text.trim();
        let source = self.source.text.trim();
        let destination = self.destination.text.trim();
        if name.is_empty() {
            return Err("name is required".into());
        }
        if source.is_empty() {
            return Err("source address is required".into());
        }
        if destination.is_empty() {
            return Err("destination address is required".into());
        }
        if let Err(e) = shell_words::split(&self.options.text) {
            return Err(format!("options: {e}"));
        }
        Ok(TunnelConfig {
            name: name.to_string(),
            source: source.to_string(),
            destination: destination.to_string(),
            options: self.options.text.trim().to_string(),
            autostart: self.autostart,
        })
    }
}

pub enum Mode {
    Normal,
    Form(Form),
    ConfirmDelete,
    Help,
}

pub struct App {
    pub tunnels: Vec<Tunnel>,
    pub selected: usize,
    pub mode: Mode,
    pub show_log: bool,
    pub should_quit: bool,
    pub status_msg: Option<(String, Instant)>,
    pub config_path: PathBuf,
    pub socat_available: bool,
    tx: Sender<ProcEvent>,
    rx: Receiver<ProcEvent>,
    next_id: u64,
}

impl App {
    pub fn new(config_path: PathBuf) -> Result<Self> {
        let (tx, rx) = channel();
        let configs = config::load(&config_path)?;
        let mut next_id = 1;
        let tunnels = configs
            .into_iter()
            .map(|c| {
                let t = Tunnel::new(next_id, c);
                next_id += 1;
                t
            })
            .collect();
        let socat_available = std::process::Command::new("socat")
            .arg("-V")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        Ok(Self {
            tunnels,
            selected: 0,
            mode: Mode::Normal,
            show_log: true,
            should_quit: false,
            status_msg: None,
            config_path,
            socat_available,
            tx,
            rx,
            next_id,
        })
    }

    pub fn autostart(&mut self) {
        let tx = self.tx.clone();
        for t in self.tunnels.iter_mut().filter(|t| t.config.autostart) {
            t.start(tx.clone());
        }
        if !self.socat_available {
            self.set_status("warning: `socat` not found in PATH".into());
        }
    }

    pub fn set_status(&mut self, msg: String) {
        self.status_msg = Some((msg, Instant::now()));
    }

    pub fn status_text(&self) -> Option<&str> {
        self.status_msg
            .as_ref()
            .filter(|(_, t)| t.elapsed() < STATUS_TTL)
            .map(|(m, _)| m.as_str())
    }

    pub fn selected_tunnel(&self) -> Option<&Tunnel> {
        self.tunnels.get(self.selected)
    }

    pub fn running_count(&self) -> usize {
        self.tunnels.iter().filter(|t| t.is_running()).count()
    }

    pub fn total_rates(&self) -> (f64, f64) {
        self.tunnels
            .iter()
            .fold((0.0, 0.0), |(l, r), t| (l + t.rate_ltr, r + t.rate_rtl))
    }

    /// Pull all pending stderr lines from the socat processes.
    pub fn drain_events(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            if let Some(t) = self.tunnels.iter_mut().find(|t| t.id == ev.id) {
                t.handle_line(ev.line);
            }
        }
    }

    pub fn on_tick(&mut self) {
        let any_running = self.tunnels.iter().any(|t| t.is_running());
        let snapshot = if any_running {
            procs::snapshot()
        } else {
            Vec::new()
        };
        for t in &mut self.tunnels {
            t.tick(&snapshot);
        }
    }

    fn persist(&mut self) {
        let configs: Vec<TunnelConfig> = self.tunnels.iter().map(|t| t.config.clone()).collect();
        if let Err(e) = config::save(&self.config_path, &configs) {
            self.set_status(format!("failed to save config: {e}"));
        }
    }

    fn clamp_selection(&mut self) {
        if self.tunnels.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.tunnels.len() {
            self.selected = self.tunnels.len() - 1;
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => self.on_key_normal(key),
            Mode::Form(form) => self.on_key_form(form, key),
            Mode::ConfirmDelete => self.on_key_confirm(key),
            Mode::Help => {} // any key closes help
        }
    }

    fn on_key_normal(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') | KeyCode::F(1) => self.mode = Mode::Help,
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.tunnels.is_empty() {
                    self.selected = (self.selected + 1) % self.tunnels.len();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if !self.tunnels.is_empty() {
                    self.selected = (self.selected + self.tunnels.len() - 1) % self.tunnels.len();
                }
            }
            KeyCode::Home | KeyCode::Char('g') => self.selected = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.selected = self.tunnels.len().saturating_sub(1)
            }
            KeyCode::Char('a') | KeyCode::Char('n') => self.mode = Mode::Form(Form::new()),
            KeyCode::Char('e') | KeyCode::Enter => {
                if let Some(t) = self.tunnels.get(self.selected) {
                    self.mode = Mode::Form(Form::from_config(t.id, &t.config));
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if !self.tunnels.is_empty() {
                    self.mode = Mode::ConfirmDelete;
                }
            }
            KeyCode::Char('s') | KeyCode::Char(' ') => self.toggle_selected(),
            KeyCode::Char('r') => self.restart_selected(),
            KeyCode::Char('K') => {
                if let Some(t) = self.tunnels.get_mut(self.selected) {
                    t.kill_now();
                }
            }
            KeyCode::Char('S') => {
                let tx = self.tx.clone();
                for t in self.tunnels.iter_mut().filter(|t| !t.is_running()) {
                    t.start(tx.clone());
                }
                self.set_status("started all tunnels".into());
            }
            KeyCode::Char('X') => {
                for t in self.tunnels.iter_mut() {
                    t.stop();
                }
                self.set_status("stopping all tunnels".into());
            }
            KeyCode::Char('c') => {
                if let Some(t) = self.tunnels.get_mut(self.selected) {
                    t.clear_stats();
                }
            }
            KeyCode::Char('C') => {
                for t in self.tunnels.iter_mut() {
                    t.clear_stats();
                }
            }
            KeyCode::Char('l') => self.show_log = !self.show_log,
            KeyCode::Char('J') => self.move_selected(1),
            KeyCode::Char('U') => self.move_selected(-1),
            _ => {}
        }
    }

    fn toggle_selected(&mut self) {
        let tx = self.tx.clone();
        if let Some(t) = self.tunnels.get_mut(self.selected) {
            if t.is_running() {
                if t.status == Status::Stopping {
                    t.kill_now();
                } else {
                    t.stop();
                }
            } else {
                t.start(tx);
            }
        }
    }

    fn restart_selected(&mut self) {
        let tx = self.tx.clone();
        if let Some(t) = self.tunnels.get_mut(self.selected) {
            if t.is_running() {
                t.stop();
                if !t.wait_exit(Duration::from_secs(2)) {
                    t.kill_now();
                    t.wait_exit(Duration::from_secs(1));
                }
            }
            t.start(tx);
        }
    }

    fn move_selected(&mut self, delta: i32) {
        if self.tunnels.len() < 2 {
            return;
        }
        let new = self.selected as i32 + delta;
        if new < 0 || new >= self.tunnels.len() as i32 {
            return;
        }
        self.tunnels.swap(self.selected, new as usize);
        self.selected = new as usize;
        self.persist();
    }

    fn on_key_form(&mut self, mut form: Form, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Enter => match form.to_config() {
                Ok(cfg) => {
                    self.commit_form(form.editing, cfg);
                    return;
                }
                Err(e) => form.error = Some(e),
            },
            KeyCode::Tab | KeyCode::Down => form.focus = (form.focus + 1) % FORM_FIELDS,
            KeyCode::BackTab | KeyCode::Up => {
                form.focus = (form.focus + FORM_FIELDS - 1) % FORM_FIELDS
            }
            KeyCode::Char(' ') if form.focus == FORM_FIELDS - 1 => form.autostart = !form.autostart,
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(i) = form.input_mut() {
                    i.clear();
                }
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(i) = form.input_mut() {
                    i.home();
                }
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(i) = form.input_mut() {
                    i.end();
                }
            }
            KeyCode::Char(c) => {
                if let Some(i) = form.input_mut() {
                    i.insert(c);
                }
            }
            KeyCode::Backspace => {
                if let Some(i) = form.input_mut() {
                    i.backspace();
                }
            }
            KeyCode::Delete => {
                if let Some(i) = form.input_mut() {
                    i.delete();
                }
            }
            KeyCode::Left => {
                if let Some(i) = form.input_mut() {
                    i.left();
                }
            }
            KeyCode::Right => {
                if let Some(i) = form.input_mut() {
                    i.right();
                }
            }
            KeyCode::Home => {
                if let Some(i) = form.input_mut() {
                    i.home();
                }
            }
            KeyCode::End => {
                if let Some(i) = form.input_mut() {
                    i.end();
                }
            }
            _ => {}
        }
        self.mode = Mode::Form(form);
    }

    fn commit_form(&mut self, editing: Option<u64>, cfg: TunnelConfig) {
        match editing {
            Some(id) => {
                if let Some(t) = self.tunnels.iter_mut().find(|t| t.id == id) {
                    let changed = t.config != cfg;
                    t.config = cfg;
                    if changed && t.is_running() {
                        self.set_status("saved; restart (r) to apply the new addresses".into());
                    } else {
                        self.set_status("saved".into());
                    }
                }
            }
            None => {
                let id = self.next_id;
                self.next_id += 1;
                self.tunnels.push(Tunnel::new(id, cfg));
                self.selected = self.tunnels.len() - 1;
                self.set_status("added; press s to start".into());
            }
        }
        self.persist();
    }

    fn on_key_confirm(&mut self, key: KeyEvent) {
        let confirmed = matches!(
            key.code,
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter
        );
        if !confirmed || self.selected >= self.tunnels.len() {
            return;
        }
        let mut t = self.tunnels.remove(self.selected);
        if t.is_running() {
            t.stop();
            if !t.wait_exit(Duration::from_secs(2)) {
                t.kill_now();
                t.wait_exit(Duration::from_secs(1));
            }
        }
        self.set_status(format!("deleted {}", t.config.name));
        self.clamp_selection();
        self.persist();
    }

    /// Stop every running socat before the terminal is handed back.
    pub fn shutdown(&mut self) {
        for t in self.tunnels.iter_mut() {
            t.stop();
        }
        for t in self.tunnels.iter_mut() {
            if !t.wait_exit(Duration::from_secs(2)) {
                t.kill_now();
                t.wait_exit(Duration::from_secs(1));
            }
        }
    }
}
