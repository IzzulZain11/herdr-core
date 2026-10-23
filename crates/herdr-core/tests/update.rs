#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;

#[test]
fn update_from_a_file_swaps_hc_and_keeps_everything_running() {
    let mut s = Session::new();
    let dir = s.root.join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(BIN, dir.join("herdr-core")).unwrap();
    std::os::unix::fs::symlink("herdr-core", dir.join("hc")).unwrap();
    s.bin = dir.join("hc"); // run as `hc`, the way people do
    let t = run(&s, &["sh", "-c", "echo before; while read l; do echo got-$l; done"]);
    let pid = s.json(&["server", "status"])["pid"].clone();

    let r = s.json(&["update", "--from", BIN]);
    let real = std::fs::canonicalize(dir.join("herdr-core")).unwrap(); // macOS: /var is /private/var
    assert_eq!(r["path"], real.display().to_string(), "the file hc runs from, not the link: {r}");
    assert!(r["was"].as_str().unwrap().starts_with("hc ") && r["now"].as_str().unwrap().starts_with("hc "), "{r}");
    assert_eq!(r["restarted"], true);
    assert_ne!(s.json(&["server", "status"])["pid"], pid, "a new core runs");
    assert!(dir.join("hc").is_symlink(), "the link stays a link");
    s.json(&["send", &t, "after"]);
    wait_until("the terminal still answers", || text(&s, &t).contains("got-after"));

    let older = s.root.join("older");
    std::fs::write(&older, "#!/bin/sh\necho 'hc 0.0.1'\n").unwrap();
    std::fs::set_permissions(&older, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let (_, e) = s.err(&["--json", "update", "--from", older.to_str().unwrap()]);
    assert_eq!(e["error"]["kind"], "refused", "never older by accident: {e}");
    let junk = s.root.join("junk");
    std::fs::write(&junk, "not a program").unwrap();
    assert_eq!(s.err(&["--json", "update", "--from", junk.to_str().unwrap()]).1["error"]["kind"], "refused");
    assert!(
        std::fs::read_dir(&dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains(".new")),
        "no leftovers"
    );
}
