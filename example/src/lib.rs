use axum::{extract::State, response::Html, routing::get, Router};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

#[derive(Clone, Default)]
pub struct AppState {
    pub requests: Arc<AtomicU64>,
}

#[no_mangle]
pub extern "Rust" fn axum_hotreload_build_router(state: AppState) -> Router<()> {
    Router::new()
        .route("/", get(page))
        .with_state(state)
}

async fn page(State(s): State<AppState>) -> Html<String> {
    let n = s.requests.fetch_add(1, Ordering::Relaxed) + 1;
    Html(format!(
        r##"<!doctype html>
<html>
<head><title>example</title></head>
<body style="font-family:ui-sans-serif,system-ui;margin:4rem;color:#444;">
  <h1>Hello, hot world!</h1>
  <p>Edit <code>example/src/lib.rs</code> and save.</p>
  <p style="color:#888;font-size:0.85rem;">visit #{n}</p>
</body>
</html>"##
    ))
}
