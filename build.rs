//! Guarantees `ui/dist` exists before `include_dir!` embeds it, so
//! `cargo build` works without a Node toolchain. Release binaries are built
//! after `npm run build` in `ui/`, which replaces the placeholder with the
//! real web app.

use std::path::Path;

fn main() {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/dist");
    std::fs::create_dir_all(&dist).expect("creating ui/dist");
    let index = dist.join("index.html");
    if !index.exists() {
        std::fs::write(&index, PLACEHOLDER).expect("writing ui/dist placeholder");
    }
    println!("cargo:rerun-if-changed=ui/dist");
}

const PLACEHOLDER: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Undo</title>
<style>
  body { background:#09090b; color:#e4e4e7; font:15px/1.6 ui-sans-serif,system-ui,sans-serif;
         display:grid; place-items:center; min-height:100vh; margin:0; }
  main { max-width:34rem; padding:2rem; }
  code { background:#111113; border:1px solid #27272a; border-radius:6px; padding:.15rem .4rem;
         font-family:ui-monospace,SFMono-Regular,Menlo,monospace; font-size:.9em; }
  h1 { font-size:1.2rem; } p { color:#a1a1aa; }
</style>
</head>
<body>
<main>
  <h1>Undo UI assets are not bundled in this build</h1>
  <p>This binary was compiled without the web app. Build it first:</p>
  <p><code>cd ui &amp;&amp; npm install &amp;&amp; npm run build</code>, then rebuild with <code>cargo build</code>.</p>
</main>
</body>
</html>
"#;
