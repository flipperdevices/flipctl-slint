//! Generates the crate's themes from tokens.toml and compiles the components.
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

    #[cfg(feature = "screens")]
    compile_slint(&theme_slint);
}

/// Compile the .slint components against the generated theme.
///
/// `EmbedForSoftwareRenderer` pre-rasterises the glyphs at compile time, which
/// is what lets the runtime drop the font engine (and with it fontique's
/// fontconfig enumeration, the reason the installer currently ships TTFs into
/// /usr/share/fonts just to avoid a startup panic).
///
/// SLINT_FONT_SIZES is pinned to 16 and must stay there: the FlipCTL fonts are
/// pixel fonts on a 64-unit design-pixel grid at 1024 units/em, so they
/// rasterise with zero partial coverage at 16px and antialias at anything else.
#[cfg(feature = "screens")]
fn compile_slint(theme: &std::path::Path) {
    println!("cargo:rerun-if-changed=ui");

    std::env::set_var("SLINT_FONT_SIZES", "16");

    // The theme is a library path, so a component reaches it as `@theme` wherever it sits.
    let config = || {
        let mut libs = std::collections::HashMap::new();
        libs.insert("theme".to_string(), theme.to_path_buf());
        slint_build::CompilerConfiguration::new()
            .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer)
            .with_library_paths(libs)
    };

    slint_build::compile_with_config("ui/screens.slint", config())
        .expect("compile ui/screens.slint");

    // `tests/portrait_row.slint` is a fixture for `tests/portrait_ui.rs` rather than a
    // component flipctl draws, so it is compiled on its own and the test includes the
    // result by path. Through `ui/screens.slint` it would be a test fixture inside the
    // production module, drawn by flipctl and never called.
    //
    // `compile_with_output_path` prints no cargo messages, so `SLINT_INCLUDE_GENERATED`
    // still points at `ui/screens.slint` and a second entry cannot take `include_modules!`
    // over.
    println!("cargo:rerun-if-changed=tests/portrait_row.slint");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let deps = slint_build::compile_with_output_path(
        manifest.join("tests/portrait_row.slint"),
        out.join("portrait_row.rs"),
        config(),
    )
    .expect("compile tests/portrait_row.slint");
    for dep in deps {
        println!("cargo:rerun-if-changed={}", dep.display());
    }
}
