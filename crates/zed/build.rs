use std::process::Command;

fn main() {
    println!("cargo:rustc-env=MACOSX_DEPLOYMENT_TARGET=10.15.7");

    // Weakly link ReplayKit to ensure Zed can be used on macOS 10.15+.
    println!("cargo:rustc-link-arg=-Wl,-weak_framework,ReplayKit");

    // Seems to be required to enable Swift concurrency
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");

    // Register exported Objective-C selectors, protocols, etc
    println!("cargo:rustc-link-arg=-Wl,-ObjC");

    // weak link to support Catalina
    println!("cargo:rustc-link-arg=-Wl,-weak_framework,ScreenCaptureKit");

    // Populate git sha environment variable if git is available
    println!("cargo:rerun-if-changed=../../.git/logs/HEAD");
    println!(
        "cargo:rustc-env=TARGET={}",
        std::env::var("TARGET").unwrap()
    );
    if let Ok(output) = Command::new("git").args(["rev-parse", "HEAD"]).output() {
        if output.status.success() {
            let git_sha = String::from_utf8_lossy(&output.stdout);
            let git_sha = git_sha.trim();

            println!("cargo:rustc-env=ZED_COMMIT_SHA={git_sha}");

            if let Ok(build_profile) = std::env::var("PROFILE") {
                if build_profile == "release" {
                    // This is currently the best way to make `cargo build ...`'s build script
                    // to print something to stdout without extra verbosity.
                    println!(
                        "cargo:warning=Info: using '{git_sha}' hash for ZED_COMMIT_SHA env var"
                    );
                }
            }
        }
    }
}
