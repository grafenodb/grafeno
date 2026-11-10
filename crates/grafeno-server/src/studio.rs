//! The engine has no interface (phase 15.1): Grafeno Studio is installed on
//! its own, as a separate product, and connects to the engine
//! through its HTTP API. `/` and `/studio/` (where the Studio used to be)
//! say so.

use axum::response::Html;

const NOTE: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>Grafeno</title>\
<style>body{font:15px/1.6 system-ui,sans-serif;max-width:640px;margin:48px auto;padding:0 16px}\
code{background:#8882;padding:2px 6px;border-radius:4px}</style></head><body>\
<h1>Grafeno</h1>\
<p>This is a Grafeno engine. It has no interface of its own: its HTTP API is at \
<code>/v1</code>, and <code>grafeno shell</code> is its console.</p>\
<p>Grafeno Studio, the graphical interface, is installed on its own and connects to this \
server's address. Before Grafeno 15 it was built into the engine at <code>/studio/</code>.</p>\
</body></html>";

/// The note at `/` and `/studio/`.
pub async fn note() -> Html<&'static str> {
    Html(NOTE)
}
