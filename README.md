# axum-hotreload

[![Crates.io](https://img.shields.io/crates/v/axum-hotreload)](https://crates.io/crates/axum-hotreload)
[![Documentation](https://img.shields.io/docsrs/axum-hotreload)](https://docs.rs/axum-hotreload)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)

Hot reload for [axum]. Save a file → next request hits new code → browser
refreshes itself. State preserved, socket never re-bound.

![demo](https://raw.githubusercontent.com/dennj/axum-hotreload/main/video.gif)

## Use it

In your existing axum project:

**1. Add to your `Cargo.toml`:**

```toml
[lib]
crate-type = ["cdylib", "rlib"]
```

**2. Add the dependency:**

```sh
cargo add axum-hotreload
```

**3. Split your code into `src/lib.rs` and `src/main.rs`:**

```rust
// src/lib.rs — the swappable part
#[no_mangle]
pub extern "Rust" fn axum_hotreload_build_router(state: AppState) -> Router<()> {
    Router::new().route("/", get(hello)).with_state(state)
}
```

```rust
// src/main.rs — the host
#[cfg(debug_assertions)]
async fn build_app(state: AppState) -> anyhow::Result<Router> {
    let hot = axum_hotreload::HotReload::<AppState>::builder()
        .package("your-crate-name")
        .state(state)
        .build()
        .await?;
    Ok(hot.into_router())
}

#[cfg(not(debug_assertions))]
async fn build_app(state: AppState) -> anyhow::Result<Router> {
    Ok(your_crate::axum_hotreload_build_router(state))
}
```

Then `cargo run`. Edit a handler, save, watch the browser refresh itself.
`cargo run --release` strips all hot-reload code out.

Full template: [example/](example/) — 3 files, ~50 lines.

## License

MIT.

[axum]: https://github.com/tokio-rs/axum
