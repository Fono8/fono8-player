use std::process::Command;

/// Output of a git command in the source tree, or `None` (no git, not a checkout).
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string()).filter(|s| !s.is_empty())
}

fn main() {
    println!("cargo:rerun-if-changed=assets/app-icon/fono8.ico");
    println!("cargo:rerun-if-changed=build.rs");
    // The commit this build comes from, shown next to the version ("0.1.5 (51b8c52)");
    // "-dirty" when tracked files differ from it. Empty outside a git checkout.
    for path in ["HEAD", "index", "packed-refs"] {
        if let Some(file) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={file}");
        }
    }
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]).and_then(|r| git(&["rev-parse", "--git-path", &r])) {
        println!("cargo:rerun-if-changed={reference}");
    }
    let hash = git(&["rev-parse", "--short=7", "HEAD"]).map(|hash| {
        let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some();
        if dirty {
            format!("{hash}-dirty")
        } else {
            hash
        }
    });
    println!("cargo:rustc-env=FONO8_GIT_HASH={}", hash.unwrap_or_default());
    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/app-icon/fono8.ico");
        resource.set("ProductName", "Fono8");
        resource.set("FileDescription", "Fono8 - music player");
        resource.set("CompanyName", "OPEN8");
        resource.set("LegalCopyright", "MIT License");
        if let Err(error) = resource.compile() {
            println!("cargo:warning=Windows resources not embedded: {error}");
        }
    }
}
