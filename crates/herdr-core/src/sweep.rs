//! Ending everything a terminal started. Each terminal's environment carries
//! a random token that its descendants inherit, even those that leave its
//! session (agents' tool shells do), so the token finds them all. A process
//! that both leaves the session and drops its environment escapes: accepted.

#[cfg(unix)]
use std::time::{Duration, Instant};

pub const VAR: &str = "HC_SCOPE";
/// Marks hc's own cores and holders: never swept, nor anything beneath them
/// (a terminal may start another session's core, which is not its own).
pub const RUNTIME: &str = "HC_RUNTIME";

/// A fresh token for one terminal.
pub fn token() -> String {
    crate::platform::random_hex()
}

/// On Windows each terminal's job object ends everything it started when the
/// job closes (its program ended, or the holder died): nothing to hunt.
#[cfg(windows)]
pub fn sweep(_: &[String]) {}

#[cfg(windows)]
pub fn members(_: &[String]) -> Vec<i32> {
    Vec::new()
}

/// Hang up, then terminate, then kill every process carrying one of
/// `tokens`, looking again before each step (they may fork). Returns once none
/// remain or KILL had its time.
#[cfg(unix)]
pub fn sweep(tokens: &[String]) {
    for (signal, wait) in [(libc::SIGHUP, 500), (libc::SIGTERM, 500), (libc::SIGKILL, 2000)] {
        let pids = members(tokens);
        if pids.is_empty() {
            return;
        }
        for pid in pids {
            unsafe { libc::kill(pid, signal) };
        }
        let deadline = Instant::now() + Duration::from_millis(wait);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
            if members(tokens).is_empty() {
                return;
            }
        }
    }
}

/// Live processes (zombies, us and hc's own processes excluded) whose
/// environment holds `HC_SCOPE=<one of tokens>`, plus their descendants:
/// macOS hides the environment of its own binaries (an agent's `/bin/bash`
/// tool shell), but not their parents.
#[cfg(unix)]
pub fn members(tokens: &[String]) -> Vec<i32> {
    if tokens.is_empty() {
        return Vec::new();
    }
    let entries: std::collections::HashSet<Vec<u8>> =
        tokens.iter().map(|t| format!("{VAR}={t}").into_bytes()).collect();
    let runtime = format!("{RUNTIME}=1").into_bytes();
    let me = std::process::id() as i32;
    let (mut set, mut hc) = (std::collections::HashSet::new(), std::collections::HashSet::new());
    let live: Vec<(i32, i32)> = processes().into_iter().filter(|(pid, _)| *pid != me).collect();
    for &(pid, _) in &live {
        let Some(env) = environ(pid) else { continue };
        let mut vars = env.split(|b| *b == 0);
        if vars.clone().any(|e| e == runtime) {
            hc.insert(pid);
        } else if vars.any(|e| entries.contains(e)) {
            set.insert(pid);
        }
    }
    loop {
        let before = set.len();
        let children: Vec<i32> =
            live.iter().filter(|(pid, ppid)| set.contains(ppid) && !hc.contains(pid)).map(|(pid, _)| *pid).collect();
        set.extend(children);
        if set.len() == before {
            return set.into_iter().collect();
        }
    }
}

/// Every live process as (pid, parent pid); zombies left out.
#[cfg(target_os = "linux")]
fn processes() -> Vec<(i32, i32)> {
    let stat = |pid: i32| -> Option<(i32, i32)> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let mut fields = stat.rsplit_once(')')?.1.split_whitespace();
        let state = fields.next()?;
        let ppid = fields.next()?.parse().ok()?;
        (state != "Z" && state != "X").then_some((pid, ppid))
    };
    let pids =
        std::fs::read_dir("/proc").into_iter().flatten().flatten().filter_map(|e| e.file_name().to_str()?.parse().ok());
    pids.filter_map(stat).collect()
}

/// NUL-separated `KEY=value` entries; a zombie has none.
#[cfg(target_os = "linux")]
fn environ(pid: i32) -> Option<Vec<u8>> {
    std::fs::read(format!("/proc/{pid}/environ")).ok().filter(|env| !env.is_empty())
}

#[cfg(target_os = "macos")]
fn processes() -> Vec<(i32, i32)> {
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let mut pids = vec![0i32; n.max(0) as usize + 64];
    let size = (pids.len() * std::mem::size_of::<i32>()) as libc::c_int;
    let got = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), size) };
    pids.truncate(got.max(0) as usize);
    pids.into_iter()
        .filter(|&pid| pid > 0)
        .filter_map(|pid| {
            let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
            let got = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
            const SZOMB: u32 = 5;
            (got == size && info.pbi_status != SZOMB).then_some((pid, info.pbi_ppid as i32))
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn environ(pid: i32) -> Option<Vec<u8>> {
    crate::foreground::procargs(pid).map(|(_, env)| env)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn finds_only_processes_carrying_the_token() {
        let tokens = [token()];
        // Not macOS's own `sleep`: it hides its environment.
        let sleeper = |scope: &str| python("import time; time.sleep(30)").env(VAR, scope).spawn().unwrap();
        let (mut ours, mut other) = (sleeper(&tokens[0]), sleeper("someone-else"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while members(&tokens).is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(members(&tokens), [ours.id() as i32]);
        sweep(&tokens);
        assert!(ours.wait().unwrap().signal().is_some(), "it was ended");
        assert!(members(&tokens).is_empty());
        other.kill().unwrap();
        other.wait().unwrap();
    }

    #[test]
    fn a_child_that_dropped_the_token_still_counts_while_its_parent_does() {
        let tokens = [token()];
        // Like an agent running a tool shell that no longer shows the token.
        let child = "import os, subprocess; env = dict(os.environ); env.pop('HC_SCOPE'); subprocess.run(['sleep', '30'], env=env)";
        let mut parent = python(child).env(VAR, &tokens[0]).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while members(&tokens).len() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(members(&tokens).len(), 2, "the shell and its sleep");
        sweep(&tokens);
        parent.wait().unwrap();
        assert!(members(&tokens).is_empty());
    }

    use std::os::unix::process::ExitStatusExt;

    fn python(code: &str) -> std::process::Command {
        let mut cmd = std::process::Command::new("python3");
        cmd.args(["-c", code]);
        cmd
    }
}
