use std::process::Command;

fn main() {
    // The dictionary is embedded with include_bytes!. An empty file is allowed only to
    // bootstrap `ucgeocoder train-dict` (see docs/PLAN.md Phase 2).
    // census.prev.dict is the decode-only dictionary that train-dict moves aside on a
    // retrain (empty when there is none).
    for f in ["resources/census.dict", "resources/census.prev.dict"] {
        println!("cargo:rerun-if-changed={f}");
        assert!(
            std::path::Path::new(f).exists(),
            "{f} is missing; create it with `ucgeocoder train-dict` (an empty file bootstraps the build)"
        );
    }
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
