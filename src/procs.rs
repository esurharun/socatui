//! Lightweight process-tree enumeration via `ps`.
//!
//! socat spawns one child per connection when the `fork` option is used, and
//! `EXEC:`/`SYSTEM:` addresses spawn arbitrary programs. To get live transfer
//! statistics we must send SIGUSR1 to every *socat* process in the tree, but
//! not to the exec'd programs (whose default SIGUSR1 disposition is to die).

use std::collections::HashMap;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub comm: String,
}

/// Snapshot of all processes on the system (pid, ppid, command name).
pub fn snapshot() -> Vec<ProcInfo> {
    let out = match Command::new("ps")
        .args(["-axo", "pid=,ppid=,comm="])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let comm = it.collect::<Vec<_>>().join(" ");
            Some(ProcInfo { pid, ppid, comm })
        })
        .collect()
}

fn is_socat(comm: &str) -> bool {
    let base = comm.rsplit('/').next().unwrap_or(comm);
    base == "socat" || base.starts_with("socat")
}

/// All socat processes in the tree rooted at `root` (root always included),
/// in breadth-first order.
pub fn socat_tree(procs: &[ProcInfo], root: u32) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<&ProcInfo>> = HashMap::new();
    for p in procs {
        children.entry(p.ppid).or_default().push(p);
    }
    let mut result = vec![root];
    let mut queue = vec![root];
    while let Some(pid) = queue.pop() {
        if let Some(kids) = children.get(&pid) {
            for k in kids {
                if is_socat(&k.comm) {
                    result.push(k.pid);
                }
                queue.push(k.pid);
            }
        }
    }
    result
}
