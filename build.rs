//! Bake the browser renderer into the binary.
//!
//! `viewer/viewer.ts` (TypeScript) is the source; the binary embeds the compiled
//! `viewer.js` via `include_str!(concat!(env!("OUT_DIR"), "/viewer.js"))`. When a
//! local toolchain is present (`viewer/node_modules/.bin/tsc`, i.e. after
//! `npm ci` in `viewer/`) we compile fresh into `OUT_DIR`; otherwise we fall back
//! to the committed `viewer/dist/viewer.js`. So `cargo build`/`cargo test` work on
//! any host (no Node required) and the Docker/release builds stay toolchain-free —
//! CI is what type-checks, tests, and keeps the committed `dist/` regenerated.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    let dest = out_dir.join("viewer.js");
    let committed = Path::new("viewer/dist/viewer.js");

    println!("cargo:rerun-if-changed=viewer/viewer.ts");
    println!("cargo:rerun-if-changed=viewer/tsconfig.json");
    println!("cargo:rerun-if-changed=viewer/dist/viewer.js");

    // tsc is a shell wrapper on Windows; extension-less path works on Unix, which is
    // all we build from here.
    let tsc = Path::new("viewer/node_modules/.bin/tsc");
    let compiled = tsc.exists()
        && Command::new(tsc)
            .current_dir("viewer")
            .arg("--outDir")
            .arg(&out_dir)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

    if compiled && dest.exists() {
        return;
    }

    // Fallback: the committed prebuilt renderer.
    std::fs::copy(committed, &dest).unwrap_or_else(|e| {
        panic!(
            "no local tsc and cannot read committed {}: {e} — run `npm ci && npm run build` in viewer/",
            committed.display()
        )
    });
}
