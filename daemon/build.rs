// rust-embed requires the embedded folder to exist at compile time. The web UI
// build (`pnpm build` → ../dist) may not have run yet in a fresh checkout, so
// make sure the directory is present (empty is fine — the daemon then serves
// the RPC API only and the UI comes from Vite/Tauri in dev).
fn main() {
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../dist");
    let _ = std::fs::create_dir_all(&dist);
    println!("cargo:rerun-if-changed=../dist");
}
