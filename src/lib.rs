//! Hot-reload for [`axum`].
//!
//! Live-swap an axum [`Router`](axum::Router) at runtime by rebuilding a cdylib crate
//! containing your handlers and re-loading it. State owned by the host
//! survives every reload; in-flight requests finish on the generation
//! they started on; the listening socket is never re-bound.
//!
//! # Setup
//!
//! Split your project into three crates:
//!
//! * **host** — the binary. Owns the tokio runtime, the listener, and your
//!   `AppState`. Depends on `axum-hotreload`.
//! * **handlers** — a `cdylib` that exports the swappable router.
//! * **shared** — a regular library crate containing `AppState` (or any other
//!   types that cross the host/handlers boundary).
//!
//! The handlers crate must export a function with this exact signature:
//!
//! ```ignore
//! #[no_mangle]
//! pub extern "Rust" fn axum_hotreload_build_router(
//!     state: shared::AppState,
//! ) -> axum::Router<()> {
//!     // ...
//! }
//! ```
//!
//! # Usage
//!
//! ```ignore
//! use axum_hotreload::HotReload;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let state = shared::AppState::new();
//!
//!     let hot = HotReload::<shared::AppState>::builder()
//!         .package("handlers")
//!         .state(state)
//!         .build()
//!         .await?;
//!
//!     let app = axum::Router::new()
//!         .fallback_service(hot.service());
//!
//!     let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
//!     axum::serve(listener, app).await?;
//!     Ok(())
//! }
//! ```

mod live;
mod watcher;

pub use live::{LiveService, ReloadStatus};

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

/// The exact symbol name that the handlers cdylib must export.
///
/// Signature: `extern "Rust" fn(S) -> axum::Router<()>` where `S` is the
/// state type configured on the [`HotReload`] builder.
pub const BUILD_ROUTER_SYMBOL: &[u8] = b"axum_hotreload_build_router";

/// Errors produced by the reloader.
#[derive(Debug, thiserror::Error)]
pub enum HotReloadError {
    #[error("cargo build failed:\n{0}")]
    Build(String),

    #[error("dylib not found at {0}")]
    DylibMissing(PathBuf),

    #[error("dlopen failed: {0}")]
    Dlopen(#[source] libloading::Error),

    #[error("required symbol `{0}` missing from handlers dylib")]
    SymbolMissing(String),

    #[error("panic in `axum_hotreload_build_router`")]
    BuildRouterPanic,

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("watcher error: {0}")]
    Notify(#[from] notify::Error),
}

/// Handle returned by [`HotReload::builder`].
///
/// Cloning is cheap (it's `Arc`-shared internally). The simplest use is
/// [`into_router`](Self::into_router) which returns a fully-wired
/// `axum::Router<()>` you can pass straight to `axum::serve`.
pub struct HotReload<S: Clone + Send + Sync + 'static> {
    live: LiveService<S>,
    /// Tower layer that injects a `<script>` into HTML responses and serves
    /// the long-poll endpoint for browser auto-refresh.
    browser_layer: tower_livereload::LiveReloadLayer,
}

impl<S: Clone + Send + Sync + 'static> Clone for HotReload<S> {
    fn clone(&self) -> Self {
        Self {
            live: self.live.clone(),
            browser_layer: self.browser_layer.clone(),
        }
    }
}

impl<S: Clone + Send + Sync + 'static> HotReload<S> {
    pub fn builder() -> HotReloadBuilder<S> {
        HotReloadBuilder::new()
    }

    /// The live `tower::Service` that dispatches into the swappable handlers.
    /// Mount it with `Router::fallback_service` if you want to compose with
    /// your own routes. Prefer [`into_router`](Self::into_router) otherwise.
    pub fn service(&self) -> LiveService<S> {
        self.live.clone()
    }

    /// Manually trigger a rebuild + reload. Returns when the swap is done
    /// or the build failed.
    pub async fn reload(&self) -> Result<(), HotReloadError> {
        self.live.reload().await
    }

    /// Snapshot of the last build result and generation counter.
    pub fn status(&self) -> ReloadStatus {
        self.live.status()
    }

    /// Return a fully-wired `axum::Router<()>` with:
    ///
    /// * the swappable handlers under all routes (as the fallback service)
    /// * `/__hot/status` reporting reloader state
    /// * a script-injection layer that auto-refreshes connected browsers
    ///   on every successful swap
    ///
    /// You can pass the result straight to `axum::serve`:
    ///
    /// ```ignore
    /// let app = hot.into_router();
    /// axum::serve(listener, app).await?;
    /// ```
    ///
    /// If you need to add your own routes, do so on the result via
    /// `.route()` / `.merge()`; the layer applies to them too.
    pub fn into_router(self) -> axum::Router<()> {
        use axum::routing::get;
        let live = self.service();
        let browser_layer = self.browser_layer.clone();
        axum::Router::new()
            .route("/__hot/status", get(status_handler::<S>))
            .fallback_service(live)
            .layer(browser_layer)
            .with_state(self)
    }
}

/// Builder for [`HotReload`].
pub struct HotReloadBuilder<S: Clone + Send + Sync + 'static> {
    package: Option<String>,
    workspace_root: Option<PathBuf>,
    watch_dirs: Vec<PathBuf>,
    state: Option<S>,
    debounce: Duration,
    cargo: PathBuf,
    enable_watcher: bool,
}

impl<S: Clone + Send + Sync + 'static> HotReloadBuilder<S> {
    fn new() -> Self {
        Self {
            package: None,
            workspace_root: None,
            watch_dirs: Vec::new(),
            state: None,
            debounce: Duration::from_millis(250),
            cargo: PathBuf::from("cargo"),
            enable_watcher: true,
        }
    }

    /// The cargo package name of the cdylib to rebuild on change. Required.
    pub fn package(mut self, name: impl Into<String>) -> Self {
        self.package = Some(name.into());
        self
    }

    /// Workspace root (the dir containing `Cargo.toml`). Defaults to
    /// [`std::env::current_dir`] at the time `build()` is called.
    pub fn workspace_root(mut self, p: impl Into<PathBuf>) -> Self {
        self.workspace_root = Some(p.into());
        self
    }

    /// Directory to watch for changes that should trigger a rebuild.
    ///
    /// May be called multiple times to watch multiple directories. If never
    /// called, defaults to `{workspace_root}/{package}/src`.
    pub fn watch_dir(mut self, p: impl Into<PathBuf>) -> Self {
        self.watch_dirs.push(p.into());
        self
    }

    /// State passed to `axum_hotreload_build_router` on every reload. Required.
    pub fn state(mut self, s: S) -> Self {
        self.state = Some(s);
        self
    }

    /// Debounce window for collapsing rapid filesystem events. Default 250ms.
    pub fn debounce(mut self, d: Duration) -> Self {
        self.debounce = d;
        self
    }

    /// Override the cargo executable used for rebuilds (e.g. `cargo-zigbuild`).
    /// Defaults to `cargo`.
    pub fn cargo(mut self, c: impl Into<PathBuf>) -> Self {
        self.cargo = c.into();
        self
    }

    /// Disable the file watcher. Useful if you want to drive reloads only
    /// from your own logic via [`HotReload::reload`].
    pub fn without_watcher(mut self) -> Self {
        self.enable_watcher = false;
        self
    }

    /// Run the initial build, load the first generation, and start the watcher.
    pub async fn build(self) -> Result<HotReload<S>, HotReloadError> {
        let package = self
            .package
            .expect("HotReloadBuilder::package is required");
        let state = self.state.expect("HotReloadBuilder::state is required");
        let workspace_root = self
            .workspace_root
            .unwrap_or_else(|| std::env::current_dir().expect("cwd"));
        let watch_dirs = if self.watch_dirs.is_empty() {
            // Auto-detect source directory:
            //   - workspace layout:  {ws}/{package}/src       (e.g. examples/)
            //   - single crate:      {ws}/src                 (package == workspace root)
            let nested = workspace_root.join(&package).join("src");
            let root = workspace_root.join("src");
            let chosen = if nested.is_dir() {
                nested
            } else if root.is_dir() {
                root
            } else {
                nested // fall back so the error mentions the conventional path
            };
            vec![chosen]
        } else {
            self.watch_dirs
        };

        let config = Arc::new(live::ReloadConfig {
            package: package.clone(),
            workspace_root: workspace_root.clone(),
            cargo: self.cargo.clone(),
        });

        // Build the browser-side livereload layer up front so we can hand
        // its Reloader to LiveService — every successful swap will then
        // ping any connected browsers automatically.
        let browser_layer = tower_livereload::LiveReloadLayer::new();
        let browser_reloader = browser_layer.reloader();

        // Initial build (synchronous).
        live::run_cargo_build(&config).await?;
        let live = LiveService::bootstrap(state, config, Some(browser_reloader))?;

        if self.enable_watcher {
            watcher::spawn(live.clone(), watch_dirs, self.debounce)?;
        }

        Ok(HotReload {
            live,
            browser_layer,
        })
    }
}

/// An axum handler that returns the current [`ReloadStatus`] as JSON-ish text.
///
/// Mount it like:
///
/// ```ignore
/// .route("/__hot/status", axum::routing::get(axum_hotreload::status_handler::<MyState>))
/// .with_state(hot)
/// ```
pub async fn status_handler<S: Clone + Send + Sync + 'static>(
    axum::extract::State(hot): axum::extract::State<HotReload<S>>,
) -> String {
    let s = hot.status();
    format!(
        "generation: {}\nlast_success_ago: {:?}\nlast_error: {}\n",
        s.generation,
        s.last_success.map(|i| i.elapsed()),
        s.last_error.unwrap_or_else(|| "<none>".to_string()),
    )
}


/// Convenience: find the dylib that cargo would produce for `package`,
/// given a workspace root. Mainly exposed for tests/debugging.
pub fn dylib_path(workspace_root: &Path, package: &str) -> PathBuf {
    live::built_dylib_path(workspace_root, package)
}

