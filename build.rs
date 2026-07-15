// Stamp the headroom-core version this hook is BUILT AGAINST into the binary, so `headroom-hook`
// reports both its own version and the headroom-core git ref (tag when pinned to a release, else a
// short commit) it was compiled with. Sourced from Cargo.lock, so it always matches the actual build.
use std::fs;

fn main() {
    println!("cargo:rerun-if-changed=Cargo.lock");
    let r = fs::read_to_string("Cargo.lock")
        .ok()
        .and_then(|s| headroom_ref(&s))
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=HEADROOM_CORE_REF={r}");
}

/// Extract the git ref headroom-core is pinned to from its Cargo.lock `source` line. A tag pin
/// (`?tag=v0.32.0#...`) reports `v0.32.0`; a commit pin (`?rev=<sha>#...`) reports `<short> (commit)`.
fn headroom_ref(lock: &str) -> Option<String> {
    let mut in_block = false;
    for line in lock.lines() {
        let l = line.trim();
        if l == "[[package]]" {
            in_block = false;
        } else if l == "name = \"headroom-core\"" {
            in_block = true;
        } else if in_block && l.starts_with("source = ") {
            let q = l.find('?')?;
            let after = &l[q + 1..];
            let end = after.find(['#', '"']).unwrap_or(after.len());
            let qs = &after[..end]; // e.g. `tag=v0.32.0` or `rev=cdba2ecc...`
            if let Some(tag) = qs.strip_prefix("tag=") {
                return Some(tag.to_string());
            }
            if let Some(rev) = qs.strip_prefix("rev=") {
                let short = &rev[..7.min(rev.len())];
                return Some(format!("{short} (commit)"));
            }
            return None;
        }
    }
    None
}
