use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=UCG_GIT_SHA");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");
    let sha = std::env::var("UCG_GIT_SHA")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            Command::new("git")
                .args(["rev-parse", "--short=12", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=UCG_GIT_SHA={sha}");
}
