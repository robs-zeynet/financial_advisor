// Re-embed the client whenever `trunk build` rewrites dist/ (rust-embed reads
// it at compile time in release builds).
fn main() {
    println!("cargo:rerun-if-changed=../../dist");
}
