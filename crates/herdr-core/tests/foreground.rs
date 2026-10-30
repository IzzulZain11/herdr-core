#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;
use serde_json::{Value, json};
use std::io::BufRead;

#[test]
fn returning_to_the_shell_ends_the_foreground_programs_status() {
    for menu in [false, true] {
        let mut s = Session::new();
        s.env.push(("HC_FOREGROUND_MS".into(), "50".into()));
        let reporter = s.root.join("reporter.sh");
        std::fs::write(&reporter, script(&s, "r state=working:app=fake; r state=idle:id=child:app=fake; w quit"))
            .unwrap();
        let t = if menu {
            run(&s, &["bash", "-ic", "\"$@\"; exec \"$0\" -i", "bash", "sh", reporter.to_str().unwrap()])
        } else {
            let t = run(&s, &["bash", "--noprofile", "--norc", "-i"]);
            s.json(&["send", &t, &format!("sh {}", reporter.display())]);
            t
        };
        wait_until("reporter in front with root and child status", || {
            let rec = get(&s, &t);
            rec["foreground"]["pid"].is_i64()
                && rec["foreground"]["pid"] != rec["pid"]
                && rec["status"]["app"] == "fake"
                && rec["records"]["child"]["app"] == "fake"
        });
        let mut conn = s.connect();
        conn.call(1, "events.subscribe", json!({"terminal": t}));
        touch(&s, "quit");
        wait_until(&format!("shell in front without stale status (menu={menu})"), || {
            let rec = get(&s, &t);
            rec["foreground"]["pid"] == rec["pid"] && rec["status"].is_null() && rec["reported"] == false
        });
        let rec = get(&s, &t);
        assert!(rec["records"].as_object().is_none_or(|r| r.is_empty()), "{rec}");
        assert_eq!(rec["turn"], 1);
        let event = next_event(&mut conn, |m| m["params"]["type"] == "status");
        assert!(event["params"]["data"]["status"].is_null(), "{event}");
        assert_eq!(event["params"]["data"]["reported"], false);
    }
}

#[test]
fn a_core_restart_keeps_the_observed_foreground_until_the_shell_returns() {
    let mut s = Session::new();
    s.env.push(("HC_FOREGROUND_MS".into(), "50".into()));
    let reporter = s.root.join("reporter.sh");
    std::fs::write(&reporter, script(&s, "r state=working:app=reporter; w quit")).unwrap();
    let t = run(&s, &["bash", "--noprofile", "--norc", "-i"]);
    s.json(&["send", &t, &format!("sh {}", reporter.display())]);
    wait_until("reporter observed before restart", || {
        let rec = get(&s, &t);
        rec["foreground"]["pid"].is_i64()
            && rec["foreground"]["pid"] != rec["pid"]
            && rec["status"]["app"] == "reporter"
    });
    s.env.iter_mut().find(|(key, _)| key == "HC_FOREGROUND_MS").unwrap().1 = "1000".into();
    s.json(&["server", "restart"]);
    touch(&s, "quit");
    wait_until("shell clears the status saved before restart", || {
        let rec = get(&s, &t);
        rec["foreground"]["pid"] == rec["pid"] && rec["status"].is_null() && rec["reported"] == false
    });
}

#[test]
fn a_pipeline_with_an_exited_leader_ends_its_status_when_the_shell_returns() {
    let mut s = Session::new();
    s.env.push(("HC_FOREGROUND_MS".into(), "50".into()));
    let reporter = s.root.join("reporter.sh");
    let tail = s.root.join("tail.sh");
    std::fs::write(&reporter, script(&s, "r state=working:app=reporter >&2; w leader-quit")).unwrap();
    std::fs::write(&tail, script(&s, "w tail-quit")).unwrap();
    let t = run(&s, &["bash", "--noprofile", "--norc", "-i"]);
    s.json(&["send", &t, &format!("sh {} | sh {}", reporter.display(), tail.display())]);
    wait_until("reporting pipeline leader observed", || {
        let rec = get(&s, &t);
        rec["foreground"]["pid"].is_i64()
            && rec["foreground"]["pid"] != rec["pid"]
            && rec["status"]["app"] == "reporter"
    });
    touch(&s, "leader-quit");
    wait_until("leader gone while pipeline still reports working", || {
        let rec = get(&s, &t);
        rec["foreground"].is_null() && rec["reported"] == true && rec["status"]["state"] == "working"
    });
    touch(&s, "tail-quit");
    wait_until("shell in front without dead pipeline status", || {
        let rec = get(&s, &t);
        rec["foreground"]["pid"] == rec["pid"] && rec["status"].is_null() && rec["reported"] == false
    });
}

#[test]
fn what_runs_in_a_terminal_and_where_is_on_its_record_and_evented() {
    let mut s = Session::new();
    s.env.push(("HC_FOREGROUND_MS".into(), "50".into()));
    let sub = s.root.join("sub");
    std::fs::create_dir(&sub).unwrap();
    let t = run(&s, &["sh", "-c", &script(&s, &format!("w go; cd {}; exec sleep 100", sub.display()))]);
    let canonical = |p: &str| std::fs::canonicalize(p).unwrap();
    // By its argv: the OS names macOS's /bin/sh `bash`, and that's the fact we pass on.
    wait_until("the shell in front", || get(&s, &t)["foreground"]["argv"][0] == "sh");
    let fg = get(&s, &t)["foreground"].clone();
    assert_eq!(canonical(fg["cwd"].as_str().unwrap()), canonical(get(&s, &t)["cwd"].as_str().unwrap()), "{fg}");
    assert!(fg["pid"].as_i64().unwrap() > 0 && !fg["name"].as_str().unwrap().is_empty(), "{fg}");

    let mut conn = s.connect();
    conn.call(1, "events.subscribe", json!({"terminal": t}));
    touch(&s, "go");
    let event = loop {
        let mut line = String::new();
        conn.reader.read_line(&mut line).expect("an event within 10 s");
        let msg: Value = serde_json::from_str(&line).unwrap();
        if msg["params"]["type"] == "foreground" && msg["params"]["data"]["name"] == "sleep" {
            break msg["params"]["data"].clone();
        }
    };
    assert_eq!(event["argv"], json!(["sleep", "100"]));
    assert_eq!(canonical(event["cwd"].as_str().unwrap()), canonical(sub.to_str().unwrap()));
    assert_eq!(get(&s, &t)["foreground"], event, "the record says the same");
}
