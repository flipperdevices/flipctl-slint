//! Compile this program's own screen against flipctl's widget library.
//!
//! The same two library paths a hosted app gets, and the same generated theme, from
//! the same tokens.toml: `@flipctl` is crates/flipper-ui/ui and `@theme` is the
//! theme written here. What this does not do is compile flipper-ui's own entry file,
//! which is Root and every screen with it.

use std::collections::HashMap;
use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let ui = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("the workspace root")
        .join("crates/flipper-ui");

    println!("cargo:rerun-if-changed={}", ui.join("tokens.toml").display());
    println!("cargo:rerun-if-changed={}", ui.join("ui").display());
    println!("cargo:rerun-if-changed=ui");

    let doc = flipper_tokens::parse(
        &std::fs::read_to_string(ui.join("tokens.toml")).expect("tokens.toml"),
    );
    let theme = out.join("theme.slint");
    flipper_tokens::write_if_changed(&theme, &flipper_tokens::slint_theme(&doc));

    // Pinned at 16, and it must stay there: the panel's fonts are pixel fonts that
    // rasterise with zero partial coverage at 16px and antialias at any other size.
    std::env::set_var("SLINT_FONT_SIZES", "16");

    let libs = HashMap::from([
        ("theme".to_string(), theme),
        ("flipctl".to_string(), ui.join("ui")),
    ]);
    let config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer)
        .with_library_paths(libs);
    slint_build::compile_with_config("ui/menu.slint", config)
        .unwrap_or_else(|e| panic!("compile ui/menu.slint: {e}"));
}
