//! Records which commit this flipctl was built from, for Settings > System info.
//!
//! `FLIPCTL_COMMIT` wins when it is set: `build_deploy.sh` sends a source tree with no
//! `.git` to the device, so it works the commit out on the host and passes it in.
//! Otherwise git is asked, which is how the image build, a clone, gets it.
//!
//! Only files that exist are watched. A path that does not exist makes cargo run this
//! again on every build, and no `rerun-if-changed` at all makes it run on any change to
//! the package.

use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=FLIPCTL_COMMIT");
    let given = std::env::var("FLIPCTL_COMMIT").ok().filter(|c| !c.is_empty());
    let commit = given.or_else(|| {
        let dir = git(&["rev-parse", "--absolute-git-dir"])?;
        let head = Path::new(&dir).join("HEAD");
        let mut watch = vec![head.clone()];
        if let Some(name) = std::fs::read_to_string(&head)
            .ok()
            .and_then(|h| h.trim().strip_prefix("ref: ").map(str::to_string))
        {
            watch.push(Path::new(&dir).join(name));
            watch.push(Path::new(&dir).join("packed-refs"));
        }
        for path in watch.iter().filter(|p| p.exists()) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
        git(&["rev-parse", "--short=12", "HEAD"])
    });
    println!("cargo:rustc-env=FLIPCTL_COMMIT={}", commit.as_deref().unwrap_or("unknown"));
}
