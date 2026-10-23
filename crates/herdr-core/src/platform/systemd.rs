//! Linux: cores and holders run as systemd user services. They belong to the
//! user, not to whatever started them: a desktop crash, a logout or an ssh
//! login ending can't take them along, and an ssh login's environment never
//! becomes theirs.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What the login session decides. Programs get it from the user manager as
/// it is now; a service takes the caller's only when the caller isn't an ssh
/// login (desktops that don't share theirs with systemd: i3, startx).
const SESSION: &[&str] = &[
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "DBUS_SESSION_BUS_ADDRESS",
    "SSH_AUTH_SOCK",
    "XDG_SESSION_TYPE",
    "XDG_SESSION_CLASS",
    "XDG_SESSION_DESKTOP",
    "XDG_SESSION_ID",
    "XDG_CURRENT_DESKTOP",
    "DESKTOP_SESSION",
];

/// What systemd sets for a service: not a program's business, and a service
/// starting another must not pass its own on.
pub const SERVICE: &[&str] = &[
    "INVOCATION_ID",
    "JOURNAL_STREAM",
    "SYSTEMD_EXEC_PID",
    "MANAGERPID",
    "MANAGERPIDFDID",
    "MEMORY_PRESSURE_WATCH",
    "MEMORY_PRESSURE_WRITE",
    "NOTIFY_SOCKET",
    "WATCHDOG_PID",
    "WATCHDOG_USEC",
    "LISTEN_PID",
    "LISTEN_FDS",
    "LISTEN_FDNAMES",
];

/// The user's runtime dir when this process uses it and a user manager runs
/// there. Err(true): the default runtime dir but no manager (warn); Err(false):
/// a custom runtime dir (isolated sessions, tests), which stays on its own.
fn manager() -> Result<PathBuf, bool> {
    let run = PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }));
    let ours = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()).map_or_else(|| run.clone(), PathBuf::from);
    match (ours == run, run.join("systemd/private").exists()) {
        (true, true) => Ok(run),
        (default, _) => Err(default),
    }
}

const NO_MANAGER: &str = "hc: no systemd user manager here: the core ends with whatever started it";

/// Starts `cmd` as a user service with stderr appended to `log`. None: no
/// user manager here, start it the plain way.
pub fn spawn(cmd: &Command, log: &Path) -> Option<io::Result<()>> {
    let run = match manager() {
        Ok(run) => run,
        Err(warn) => {
            if warn {
                eprintln!("{NO_MANAGER}");
            }
            return None;
        }
    };
    let mut caller: BTreeMap<String, String> = super::utf8_vars().collect();
    let mut unset: Vec<&str> = super::SSH_LOGIN.to_vec();
    for (k, v) in cmd.get_envs() {
        let Some(k) = k.to_str() else { continue };
        match v.and_then(OsStr::to_str) {
            Some(v) => caller.insert(k.into(), v.into()),
            None => {
                unset.push(k);
                caller.remove(k)
            }
        };
    }
    let mut env = service_env(caller);
    env.insert("XDG_RUNTIME_DIR".into(), run.display().to_string());
    let args: Vec<&OsStr> = cmd.get_args().collect();
    let name = format!("hc-{}-{}", args.first().and_then(|a| a.to_str()).unwrap_or("run"), &super::random_hex()[..8]);
    let here = std::env::current_dir().is_ok_and(|d| d.is_dir());
    // Values ride in systemd-run's own environment: its arguments are world-readable.
    let out = Command::new("systemd-run")
        .env_clear()
        .envs(&env)
        .args(unit_args(&name, cmd.get_program(), &args, env.keys(), &unset, log, here))
        .stdin(Stdio::null())
        .output();
    let started = match out {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(io::Error::other(format!("systemd-run: {}", String::from_utf8_lossy(&out.stderr).trim()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            eprintln!("{NO_MANAGER} (no systemd-run on PATH)");
            return None;
        }
        Err(e) => Err(io::Error::new(e.kind(), format!("systemd-run: {e}"))),
    };
    if started.is_ok() {
        linger();
    }
    Some(started)
}

/// The caller's environment as a service gets it: no ssh login's session, no
/// service's, and only names systemd accepts (a bash function's isn't one).
fn service_env(mut env: BTreeMap<String, String>) -> BTreeMap<String, String> {
    let ssh = super::SSH_LOGIN.iter().any(|k| env.contains_key(*k));
    let valid = |k: &str| {
        k.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    env.retain(|k, _| {
        valid(k)
            && (!ssh || !SESSION.contains(&k.as_str()))
            && !SERVICE.contains(&k.as_str())
            && !super::SSH_LOGIN.contains(&k.as_str())
    });
    env
}

/// systemd-run's arguments for one hc service, its environment by name only.
/// OOMPolicy: a program killed for memory must not stop the whole service.
/// No ManagedOOMPreference: systemd rewrites its cgroup xattr on every property
/// change, and uresourced < 0.5.5 answers each rewrite with another change,
/// flooding the user manager until it starts nothing. KillMode: when the core
/// or holder exits, nothing else goes with it.
fn unit_args<'a>(
    name: &str,
    program: &OsStr,
    args: &[&OsStr],
    names: impl Iterator<Item = &'a String>,
    unset: &[&str],
    log: &Path,
    here: bool,
) -> Vec<OsString> {
    let mut out: Vec<OsString> = ["--user", "--quiet", "--collect", "--service-type=exec"].map(OsString::from).into();
    out.push(if here { "--same-dir" } else { "--working-directory=/" }.into());
    for p in [
        format!("--unit={name}"),
        "OOMPolicy=continue".into(),
        "KillMode=process".into(),
        "StandardOutput=null".into(),
        format!("StandardError=append:{}", log.display()),
        format!("UnsetEnvironment={}", unset.join(" ")),
    ] {
        if !p.starts_with("--") {
            out.push("-p".into());
        }
        out.push(p.into());
    }
    out.extend(names.map(|k| format!("--setenv={k}").into()));
    // systemd expands $ in a service's command line; $$ is a plain $.
    let literal = |a: &OsStr| OsString::from(a.to_string_lossy().replace('$', "$$"));
    out.push(literal(program));
    out.extend(args.iter().map(|a| literal(a)));
    out
}

/// Without linger the user manager, and every hc service with it, stops when
/// the user's last login ends.
fn linger() {
    let user = unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() {
            return;
        }
        std::ffi::CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned()
    };
    if Path::new("/var/lib/systemd/linger").join(&user).exists() {
        return;
    }
    let enabled = Command::new("loginctl")
        .args(["--no-ask-password", "enable-linger", &user])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if enabled {
        eprintln!("hc: turned on linger for {user}, so the core outlives your logins");
    } else {
        eprintln!("hc: the core stops when your last login ends; `sudo loginctl enable-linger {user}` keeps it");
    }
}

/// The user manager's environment now (a core may have started before the
/// desktop did). Blocks up to a second: call it off the core's thread.
pub fn manager_env() -> Option<BTreeMap<String, String>> {
    let run = manager().ok()?;
    let out = Command::new("busctl")
        .env("XDG_RUNTIME_DIR", run)
        .args(["--user", "--json=short", "--timeout=1", "get-property", "org.freedesktop.systemd1"])
        .args(["/org/freedesktop/systemd1", "org.freedesktop.systemd1.Manager", "Environment"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let shown: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let pairs = shown["data"].as_array()?.iter().filter_map(|v| v.as_str()?.split_once('='));
    Some(pairs.map(|(k, v)| (k.into(), v.into())).collect())
}

/// Puts the login session's variables the manager has into `env`.
pub fn apply_session(env: &mut BTreeMap<String, String>, manager: &BTreeMap<String, String>) {
    for &k in SESSION {
        if let Some(v) = manager.get(k) {
            env.insert(k.into(), v.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsStr;
    use std::path::Path;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn a_service_keeps_the_callers_paths_but_not_its_ssh_login() {
        let ssh = super::service_env(map(&[
            ("HC_SESSION", "dev"),
            ("PATH", "/home/u/.local/bin"),
            ("SSH_CONNECTION", "100.1.1.1 1 100.2.2.2 22"),
            ("SSH_AUTH_SOCK", "/tmp/ssh-x/agent.1"),
            ("INVOCATION_ID", "abc"),
            ("BASH_FUNC_f%%", "() { :; }"),
        ]));
        assert_eq!(ssh.keys().collect::<Vec<_>>(), ["HC_SESSION", "PATH"]);
        let desktop = super::service_env(map(&[("DISPLAY", ":0"), ("PATH", "/usr/bin")]));
        assert!(desktop.contains_key("DISPLAY"), "a desktop that doesn't tell systemd keeps its display");
    }

    #[test]
    fn a_service_command_line_holds_no_values_and_no_expansions() {
        let env = map(&[("API_KEY", "secret-canary")]);
        let socket = OsStr::new("/run/${USER}.sock");
        let args = super::unit_args(
            "hc-hold-1",
            OsStr::new("/bin/hc"),
            &[OsStr::new("hold"), socket],
            env.keys(),
            &["HC_SCOPE"],
            Path::new("/s/x.log"),
            false,
        );
        let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(!args.iter().any(|a| a.contains("secret-canary")), "{args:?}");
        for want in ["--setenv=API_KEY", "UnsetEnvironment=HC_SCOPE", "OOMPolicy=continue", "--working-directory=/"] {
            assert!(args.iter().any(|a| a == want), "{want} missing from {args:?}");
        }
        assert_eq!(&args[args.len() - 3..], ["/bin/hc", "hold", "/run/$${USER}.sock"]);
        assert!(!args.iter().any(|a| a.starts_with("ManagedOOM")), "uresourced loops on oomd's xattr: {args:?}");
    }

    #[test]
    fn programs_get_the_desktop_the_manager_has_now() {
        let mut env = map(&[("SSH_AUTH_SOCK", "/tmp/ssh-x/agent.1"), ("DISPLAY", ":1"), ("PATH", "/usr/bin")]);
        super::apply_session(&mut env, &map(&[("PATH", "/x"), ("SSH_AUTH_SOCK", "/run/user/1/my keys/ssh")]));
        assert_eq!(env["SSH_AUTH_SOCK"], "/run/user/1/my keys/ssh");
        assert_eq!((env["PATH"].as_str(), env["DISPLAY"].as_str()), ("/usr/bin", ":1"), "only what the manager has");
    }
}
