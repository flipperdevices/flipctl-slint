//! The pages, and OpenH264's C++ runtime linked statically.
//!
//! `cc` links libstdc++ by name, which the linker resolves to the shared library, and
//! an app may need nothing beyond libc, libm and libgcc_s. A search path holding only
//! the target compiler's `libstdc++.a` comes before the system's, so the same name
//! resolves to the archive instead.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    flipctl_app::build::pages();

    let cxx = cc::Build::new().cpp(true).get_compiler();
    let found = Command::new(cxx.path())
        .arg("-print-file-name=libstdc++.a")
        .output()
        .expect("the C++ compiler for the target");
    let archive = PathBuf::from(String::from_utf8_lossy(&found.stdout).trim());
    assert!(archive.is_absolute(), "no static libstdc++ for {}", cxx.path().display());
    let dir = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("static-cxx");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(&archive, dir.join("libstdc++.a")).unwrap();
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rerun-if-changed=build.rs");
}
