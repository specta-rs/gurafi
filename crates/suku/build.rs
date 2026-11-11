//! Assemble the offline GraphiQL page for the `vendored-graphiql` feature.
//!
//! The page is generated from the checked-in assets in `src/http/vendor/` so the repository
//! stores each minified bundle once, while the crate embeds a single self-contained HTML file.
//! Nothing is downloaded at build time.

use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=src/http/vendor");

    if env::var_os("CARGO_FEATURE_VENDORED_GRAPHIQL").is_none() {
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let vendor = manifest_dir.join("src/http/vendor");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));

    let template = read(&vendor.join("graphiql.vendored.html"));
    let html = template
        .replace(
            "/*{{GRAPHIQL_CSS}}*/",
            &read(&vendor.join("graphiql.min.css")),
        )
        .replace(
            "/*{{REACT_JS}}*/",
            &read(&vendor.join("react.production.min.js")),
        )
        .replace(
            "/*{{REACT_DOM_JS}}*/",
            &read(&vendor.join("react-dom.production.min.js")),
        )
        .replace(
            "/*{{GRAPHIQL_JS}}*/",
            &read(&vendor.join("graphiql.min.js")),
        );

    // A missing placeholder would silently serve a broken page; fail the build instead.
    for placeholder in [
        "/*{{GRAPHIQL_CSS}}*/",
        "/*{{REACT_JS}}*/",
        "/*{{REACT_DOM_JS}}*/",
        "/*{{GRAPHIQL_JS}}*/",
    ] {
        assert!(
            !html.contains(placeholder),
            "unreplaced placeholder {placeholder}"
        );
    }

    fs::write(out_dir.join("graphiql.html"), html).expect("write vendored GraphiQL page");
}

fn read(path: &PathBuf) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}
