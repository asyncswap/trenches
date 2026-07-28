//! Stamps the build with the commit it came from.
//!
//! A beta gets bug reports against builds nobody can name. A tag tells you which
//! release someone ran; the commit tells you which *build* — which matters when
//! a release is rebuilt, or when someone is running something handed to them
//! rather than downloaded.
//!
//! Everything here degrades to "unknown" rather than failing: the source is
//! shipped as a tarball to some machines, and a build that refuses to compile
//! outside a git checkout is worse than one that cannot name itself.

use std::process::Command;

fn main() {
    let hash = git(&["rev-parse", "--short=9", "HEAD"]).unwrap_or_else(|| "unknown".into());

    // A dirty tree is worth knowing about: a stack trace against a commit that
    // does not match what was actually compiled sends you looking in the wrong
    // place for hours.
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    println!("cargo:rustc-env=TRENCHES_COMMIT={hash}{}", if dirty { "-dirty" } else { "" });

    // Rebuild when HEAD moves, so the stamp cannot go stale across a checkout.
    // Both paths, because HEAD is a ref and the ref is what actually changes on
    // a commit.
    for p in ["../.git/HEAD", ".git/HEAD"] {
        if std::path::Path::new(p).exists() {
            println!("cargo:rerun-if-changed={p}");
        }
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}
