// build.rs
//
// Embeds the build-time git commit hash (env!("GIT_HASH") in src/) into the
// binary. The device footprint (TODO.md Phase 12 — F10) needs a way to
// identify exactly which commit is running on a fleet device, not just the
// Cargo.toml version number every patch build between releases shares.
use std::process::Command;

fn main() {
    let hash = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GIT_HASH={hash}");
    // Catches switching branches; a new commit on the same branch already
    // triggers a rebuild via the source change that commit contains, so
    // this doesn't need to track .git/refs/heads/* too.
    println!("cargo:rerun-if-changed=.git/HEAD");
}
