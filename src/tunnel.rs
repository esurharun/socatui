//! A "tunnel" is one socat invocation: `socat --statistics [opts] SOURCE DESTINATION`.
//!
//! socat 1.8+ prints transfer counters to stderr on exit and whenever it
//! receives SIGUSR1, one pair of lines per process:
//!
//! ```text
//! 2026/09/23 16:41:03 socat[75527] I STATISTICS: left to right: 1 packets(s), 5000 byte(s)
//! 2026/09/23 16:41:03 socat[75527] I STATISTICS: right to left: 1 packets(s), 5000 byte(s)
//! ```
//!
//! With the `fork` option every connection is handled by a child process with
//! its own counters, so we keep the latest counters per pid and sum them.

use crate::procs::{self, ProcInfo};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

const LOG_CAPACITY: usize = 500;
/// How long a vanished pid keeps its own slot before its counters are folded
/// into the archived total (guards against pid reuse while still accepting
/// the final stats lines that arrive right after exit).
const DEAD_PID_GRACE: Duration = Duration::from_secs(3);
/// After SIGTERM, escalate to SIGKILL if the process is still around.
const KILL_AFTER: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct TunnelConfig {
    pub name: String,
    /// Left socat address, e.g. `TCP-LISTEN:8080,fork,reuseaddr`
    pub source: String,
    /// Right socat address, e.g. `TCP:example.com:80`
    pub destination: String,
    /// Extra socat command-line options, e.g. `-d -d -T 30`
    #[serde(default)]
    pub options: String,
    #[serde(default)]
    pub autostart: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Counters {
    pub ltr_packets: u64,
    pub ltr_bytes: u64,
    pub rtl_packets: u64,
    pub rtl_bytes: u64,
}

impl Counters {
    fn add(&mut self, o: &Counters) {
        self.ltr_packets += o.ltr_packets;
        self.ltr_bytes += o.ltr_bytes;
        self.rtl_packets += o.rtl_packets;
        self.rtl_bytes += o.rtl_bytes;
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Stopped,
    Running,
    Stopping,
    Exited(Option<i32>),
    Failed(String),
}

impl Status {
    pub fn label(&self) -> String {
        match self {
            Status::Stopped => "stopped".into(),
            Status::Running => "running".into(),
            Status::Stopping => "stopping".into(),
            Status::Exited(Some(0)) => "exited".into(),
            Status::Exited(Some(c)) => format!("exit {c}"),
            Status::Exited(None) => "killed".into(),
            Status::Failed(_) => "failed".into(),
        }
    }
}

/// A line of stderr from a tunnel's socat process.
pub struct ProcEvent {
    pub id: u64,
    pub line: String,
}

pub enum Parsed {
    Stats {
        pid: u32,
        ltr: bool,
        packets: u64,
        bytes: u64,
    },
    Noise,
    Other,
}

/// Parse one socat stderr line.
pub fn parse_line(line: &str) -> Parsed {
    if line.contains("statistics are experimental")
        || line.contains("transfer engine not yet started")
    {
        return Parsed::Noise;
    }
    let Some(idx) = line.find("STATISTICS: ") else {
        return Parsed::Other;
    };
    let Some(pid) = extract_pid(line) else {
        return Parsed::Other;
    };
    let rest = &line[idx + "STATISTICS: ".len()..];
    let (ltr, rest) = if let Some(r) = rest.strip_prefix("left to right: ") {
        (true, r)
    } else if let Some(r) = rest.strip_prefix("right to left: ") {
        (false, r)
    } else {
        return Parsed::Other;
    };
    let mut nums = rest
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<u64>().ok());
    match (nums.next(), nums.next()) {
        (Some(packets), Some(bytes)) => Parsed::Stats {
            pid,
            ltr,
            packets,
            bytes,
        },
        _ => Parsed::Other,
    }
}

fn extract_pid(line: &str) -> Option<u32> {
    let start = line.find("socat[")? + "socat[".len();
    let end = line[start..].find(']')? + start;
    line[start..end].parse().ok()
}

struct PidEntry {
    counters: Counters,
    dead_since: Option<Instant>,
}

pub struct Tunnel {
    pub id: u64,
    pub config: TunnelConfig,
    pub status: Status,
    child: Option<Child>,
    pub pid: Option<u32>,
    pub started: Option<Instant>,
    stop_requested: Option<Instant>,
    per_pid: HashMap<u32, PidEntry>,
    archived: Counters,
    pub total: Counters,
    prev_total: Counters,
    prev_sample: Instant,
    /// Bytes per second, left→right and right→left.
    pub rate_ltr: f64,
    pub rate_rtl: f64,
    /// Currently active connections (forked socat workers).
    pub connections: usize,
    /// Distinct socat worker processes seen since stats were last cleared.
    pub sessions: u64,
    pub log: VecDeque<String>,
}

impl Tunnel {
    pub fn new(id: u64, config: TunnelConfig) -> Self {
        Self {
            id,
            config,
            status: Status::Stopped,
            child: None,
            pid: None,
            started: None,
            stop_requested: None,
            per_pid: HashMap::new(),
            archived: Counters::default(),
            total: Counters::default(),
            prev_total: Counters::default(),
            prev_sample: Instant::now(),
            rate_ltr: 0.0,
            rate_rtl: 0.0,
            connections: 0,
            sessions: 0,
            log: VecDeque::new(),
        }
    }

    pub fn is_running(&self) -> bool {
        self.child.is_some()
    }

    pub fn uptime(&self) -> Option<Duration> {
        if self.child.is_some() {
            self.started.map(|s| s.elapsed())
        } else {
            None
        }
    }

    pub fn command_line(&self) -> String {
        let mut parts = vec!["socat".to_string(), "--statistics".to_string()];
        if !self.config.options.trim().is_empty() {
            parts.push(self.config.options.trim().to_string());
        }
        parts.push(self.config.source.clone());
        parts.push(self.config.destination.clone());
        parts.join(" ")
    }

    /// Write this tunnel's retained log (with a header and stats summary) to `w`.
    /// Returns the number of log lines written.
    pub fn write_log(&self, w: &mut impl Write) -> io::Result<usize> {
        writeln!(w, "=== {} [{}] ===", self.config.name, self.status.label())?;
        writeln!(w, "command: {}", self.command_line())?;
        if let Some(pid) = self.pid {
            writeln!(w, "pid: {pid}")?;
        }
        writeln!(
            w,
            "stats: left->right {} packets / {} bytes, right->left {} packets / {} bytes, {} sessions",
            self.total.ltr_packets,
            self.total.ltr_bytes,
            self.total.rtl_packets,
            self.total.rtl_bytes,
            self.sessions
        )?;
        writeln!(w)?;
        for line in &self.log {
            writeln!(w, "{line}")?;
        }
        Ok(self.log.len())
    }

    pub fn push_log(&mut self, line: String) {
        if self.log.len() >= LOG_CAPACITY {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    pub fn start(&mut self, tx: Sender<ProcEvent>) {
        if self.child.is_some() {
            return;
        }
        let opts = match shell_words::split(&self.config.options) {
            Ok(o) => o,
            Err(e) => {
                self.status = Status::Failed(format!("bad options: {e}"));
                self.push_log(format!("cannot parse options: {e}"));
                return;
            }
        };
        let mut cmd = Command::new("socat");
        cmd.arg("--statistics")
            .args(opts)
            .arg(&self.config.source)
            .arg(&self.config.destination)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .process_group(0);

        match cmd.spawn() {
            Ok(mut child) => {
                let stderr = child.stderr.take().expect("stderr piped");
                let id = self.id;
                std::thread::spawn(move || {
                    let reader = BufReader::new(stderr);
                    for line in reader.lines() {
                        match line {
                            Ok(l) => {
                                if tx.send(ProcEvent { id, line: l }).is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                });
                let pid = child.id();
                self.pid = Some(pid);
                self.child = Some(child);
                self.started = Some(Instant::now());
                self.stop_requested = None;
                self.status = Status::Running;
                self.connections = 0;
                let cl = self.command_line();
                self.push_log(format!("started pid {pid}: {cl}"));
            }
            Err(e) => {
                self.status = Status::Failed(format!("spawn failed: {e}"));
                self.push_log(format!("failed to start socat: {e}"));
            }
        }
    }

    /// Ask the whole process group to terminate; `tick` escalates to SIGKILL.
    pub fn stop(&mut self) {
        if let (Some(pid), true) = (self.pid, self.child.is_some()) {
            let _ = kill(Pid::from_raw(-(pid as i32)), Signal::SIGTERM);
            self.stop_requested = Some(Instant::now());
            self.status = Status::Stopping;
            self.push_log(format!("sent SIGTERM to process group {pid}"));
        }
    }

    pub fn kill_now(&mut self) {
        if let (Some(pid), true) = (self.pid, self.child.is_some()) {
            let _ = kill(Pid::from_raw(-(pid as i32)), Signal::SIGKILL);
            self.stop_requested = Some(Instant::now());
            self.status = Status::Stopping;
        }
    }

    /// Blocking reap used during shutdown.
    pub fn wait_exit(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => {
                    self.child = None;
                    self.pid = None;
                    self.status = Status::Stopped;
                    return true;
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        true
    }

    pub fn clear_stats(&mut self) {
        self.per_pid.clear();
        self.archived = Counters::default();
        self.total = Counters::default();
        self.prev_total = Counters::default();
        self.prev_sample = Instant::now();
        self.rate_ltr = 0.0;
        self.rate_rtl = 0.0;
        self.sessions = 0;
    }

    pub fn handle_line(&mut self, line: String) {
        match parse_line(&line) {
            Parsed::Stats {
                pid,
                ltr,
                packets,
                bytes,
            } => {
                let is_new = !self.per_pid.contains_key(&pid);
                let entry = self.per_pid.entry(pid).or_insert(PidEntry {
                    counters: Counters::default(),
                    dead_since: None,
                });
                if ltr {
                    entry.counters.ltr_packets = packets;
                    entry.counters.ltr_bytes = bytes;
                } else {
                    entry.counters.rtl_packets = packets;
                    entry.counters.rtl_bytes = bytes;
                }
                if is_new {
                    self.sessions += 1;
                }
                self.recompute_total();
            }
            Parsed::Noise => {}
            Parsed::Other => self.push_log(line),
        }
    }

    fn recompute_total(&mut self) {
        let mut t = self.archived;
        for e in self.per_pid.values() {
            t.add(&e.counters);
        }
        self.total = t;
    }

    /// Called once per second with a fresh process snapshot.
    pub fn tick(&mut self, procs: &[ProcInfo]) {
        let now = Instant::now();
        let mut alive: HashSet<u32> = HashSet::new();

        if let Some(child) = self.child.as_mut() {
            let pid = child.id();
            match child.try_wait() {
                Ok(Some(st)) => {
                    let code = st.code();
                    self.status = if self.stop_requested.is_some() {
                        Status::Stopped
                    } else {
                        Status::Exited(code)
                    };
                    self.push_log(format!("socat {st}"));
                    self.child = None;
                    self.pid = None;
                    self.stop_requested = None;
                    self.connections = 0;
                }
                Ok(None) => {
                    if let Some(t) = self.stop_requested {
                        if t.elapsed() > KILL_AFTER {
                            let _ = kill(Pid::from_raw(-(pid as i32)), Signal::SIGKILL);
                        }
                    } else {
                        let tree = procs::socat_tree(procs, pid);
                        for p in &tree {
                            let _ = kill(Pid::from_raw(*p as i32), Signal::SIGUSR1);
                            alive.insert(*p);
                        }
                        // fork mode: every extra socat is one connection.
                        // single mode: the root itself carries the traffic
                        // once it has reported counters.
                        self.connections = if tree.len() > 1 {
                            tree.len() - 1
                        } else if self.per_pid.contains_key(&pid) {
                            1
                        } else {
                            0
                        };
                    }
                }
                Err(_) => {}
            }
        }

        // Age out pids that are gone, folding their final counters into the
        // archived total after a grace period.
        let mut folded = Counters::default();
        self.per_pid.retain(|pid, entry| {
            if alive.contains(pid) {
                entry.dead_since = None;
                true
            } else {
                let since = *entry.dead_since.get_or_insert(now);
                if now.duration_since(since) > DEAD_PID_GRACE {
                    folded.add(&entry.counters);
                    false
                } else {
                    true
                }
            }
        });
        self.archived.add(&folded);
        self.recompute_total();

        // Throughput.
        let dt = now.duration_since(self.prev_sample).as_secs_f64();
        if dt >= 0.5 {
            let d_ltr = self
                .total
                .ltr_bytes
                .saturating_sub(self.prev_total.ltr_bytes);
            let d_rtl = self
                .total
                .rtl_bytes
                .saturating_sub(self.prev_total.rtl_bytes);
            self.rate_ltr = d_ltr as f64 / dt;
            self.rate_rtl = d_rtl as f64 / dt;
            self.prev_total = self.total;
            self.prev_sample = now;
        }
        if self.child.is_none() {
            self.rate_ltr = 0.0;
            self.rate_rtl = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stats_lines() {
        let l = "2026/09/23 16:41:03 socat[75527] I STATISTICS: left to right: 1 packets(s), 5000 byte(s)";
        match parse_line(l) {
            Parsed::Stats {
                pid,
                ltr,
                packets,
                bytes,
            } => {
                assert_eq!(pid, 75527);
                assert!(ltr);
                assert_eq!(packets, 1);
                assert_eq!(bytes, 5000);
            }
            _ => panic!("expected stats"),
        }
        let r =
            "2026/09/23 16:41:03 socat[7] I STATISTICS: right to left: 12 packets(s), 34 byte(s)";
        assert!(matches!(
            parse_line(r),
            Parsed::Stats {
                ltr: false,
                packets: 12,
                bytes: 34,
                ..
            }
        ));
    }

    #[test]
    fn filters_noise() {
        assert!(matches!(
            parse_line("2026/09/23 16:41:03 socat[1] W statistics are experimental"),
            Parsed::Noise
        ));
        assert!(matches!(
            parse_line("2026/09/23 16:41:03 socat[1] W transfer engine not yet started, statistics not available"),
            Parsed::Noise
        ));
        assert!(matches!(
            parse_line("2026/09/23 16:41:03 socat[1] E bind(5, ...): Address already in use"),
            Parsed::Other
        ));
    }

    #[test]
    fn sums_per_pid_and_archives() {
        let mut t = Tunnel::new(1, TunnelConfig::default());
        t.handle_line("socat[10] I STATISTICS: left to right: 1 packets(s), 100 byte(s)".into());
        t.handle_line("socat[10] I STATISTICS: right to left: 1 packets(s), 50 byte(s)".into());
        t.handle_line("socat[11] I STATISTICS: left to right: 2 packets(s), 200 byte(s)".into());
        // same pid reporting again replaces, not adds
        t.handle_line("socat[10] I STATISTICS: left to right: 3 packets(s), 300 byte(s)".into());
        assert_eq!(t.total.ltr_bytes, 500);
        assert_eq!(t.total.rtl_bytes, 50);
        assert_eq!(t.sessions, 2);
    }
}
