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
    // all we build from here. No `current_dir`: a relative program path resolves in
    // the *child's* cwd on Unix, so pairing it with current_dir("viewer") would look
    // for viewer/viewer/… and never find tsc. `-p viewer` points at the tsconfig.
    let tsc = Path::new("viewer/node_modules/.bin/tsc");
    if tsc.exists() {
        let ok = Command::new(tsc)
            .args(["-p", "viewer", "--outDir"])
            .arg(&out_dir)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok && dest.exists() {
            return;
        }
        // A present-but-failing toolchain (type error in viewer.ts, broken install)
        // must not silently bake the stale committed dist — the edit would vanish.
        println!(
            "cargo:warning=viewer tsc failed — baking the committed viewer/dist/viewer.js, \
             which does NOT include local viewer.ts changes"
        );
    }

    // Fallback: the committed prebuilt renderer.
    std::fs::copy(committed, &dest).unwrap_or_else(|e| {
        panic!(
            "no local tsc and cannot read committed {}: {e} — run `npm ci && npm run build` in viewer/",
            committed.display()
        )
    });
}
