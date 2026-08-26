//! Generates the crate's themes from tokens.toml.
//!
//! The generators live in flipper-tokens, so a hosted app's build produces the
//! same theme from the same parse of the same file.

use std::{env, fs, path::PathBuf};

use flipper_tokens::{parse, rust_theme, slint_theme, write_if_changed};

fn main() {
    println!("cargo:rerun-if-changed=tokens.toml");

    let doc = parse(&fs::read_to_string("tokens.toml").expect("tokens.toml"));

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    write_if_changed(&out.join("theme.rs"), &rust_theme(&doc));
    let theme_slint = out.join("theme.slint");
    write_if_changed(&theme_slint, &slint_theme(&doc));

}
