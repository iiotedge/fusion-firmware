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
    // Re-run when the checked-out commit changes. `.git/HEAD` alone only catches
    // switching branches: a new commit on the SAME branch moves the branch ref
    // (`.git/refs/heads/<branch>`), not HEAD itself — and a source change rebuilds
    // the crate but does NOT re-run this script, so the hash used to stay frozen
    // at whatever commit the first build saw (a device could report an old
    // commit while running newer code). Track the ref HEAD points at, and
    // packed-refs for repos whose refs have been packed.
    println!("cargo:rerun-if-changed=.git/HEAD");
    if let Ok(head) = std::fs::read_to_string(".git/HEAD") {
        if let Some(reference) = head.trim().strip_prefix("ref: ") {
            println!("cargo:rerun-if-changed=.git/{reference}");
        }
    }
    println!("cargo:rerun-if-changed=.git/packed-refs");
}
