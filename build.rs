use std::process::Command;

fn main() {
    // Git commit SHA
    let sha = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default()
        .trim()
        .to_string();
    println!("cargo:rustc-env=FC_GIT_SHA={sha}");

    // Git branch
    let branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default()
        .trim()
        .to_string();
    println!("cargo:rustc-env=FC_GIT_BRANCH={branch}");

    // Git tag (if HEAD is tagged)
    let tag = Command::new("git")
        .args(["describe", "--tags", "--exact-match", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default()
        .trim()
        .to_string();
    println!("cargo:rustc-env=FC_GIT_TAG={tag}");

    // Dirty flag
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    println!(
        "cargo:rustc-env=FC_GIT_DIRTY={}",
        if dirty { "-dirty" } else { "" }
    );
}
