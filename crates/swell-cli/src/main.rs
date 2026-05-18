// `swell` CLI — scaffold placeholder.
//
// The substantive command surface (`gen`, `check`, `watch`, etc.)
// lands in the pipeline PR (q2) once the analyzer/scanner/codegen
// crates exist. This scaffold gives the workflow something to
// cross-compile + publish to npm so the release plumbing is wired
// before the real CLI logic arrives.

fn main() {
    eprintln!("@dialo/swell-cli — scaffold; real CLI lands in a later PR");
    std::process::exit(2);
}
