use crate::{HotReloadError, BUILD_ROUTER_SYMBOL};
use arc_swap::ArcSwap;
use axum::{body::Body, extract::Request, response::Response, Router};
use libloading::{Library, Symbol};
use std::{
    convert::Infallible,
    future::Future,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    task::{Context as TaskContext, Poll},
    time::Instant,
};
use tower::Service;

pub(crate) struct ReloadConfig {
    pub package: String,
    pub workspace_root: PathBuf,
    pub cargo: PathBuf,
}

/// A loaded handler dylib together with the `Router` it produced.
///
/// The library is dropped (and unloaded) only when no `Arc<Generation>`
/// remains. Requests in flight hold an `Arc<Generation>` for their duration,
/// so the dylib's code stays mapped until the last response is sent.
struct Generation {
    router: Router<()>,
    _lib: Library,
}

#[derive(Clone)]
pub struct LiveService<S: Clone + Send + Sync + 'static> {
    state: S,
    generation: Arc<ArcSwap<Generation>>,
    config: Arc<ReloadConfig>,
    bookkeeping: Arc<Bookkeeping>,
    /// Pinger used to tell connected browsers to refresh after a successful swap.
    browser_reloader: Option<tower_livereload::Reloader>,
}

struct Bookkeeping {
    generation_counter: AtomicU64,
    last_success: Mutex<Option<Instant>>,
    last_error: Mutex<Option<String>>,
}

/// Snapshot of the reloader's current state.
#[derive(Debug, Clone)]
pub struct ReloadStatus {
    pub generation: u64,
    pub last_success: Option<Instant>,
    pub last_error: Option<String>,
}

impl<S: Clone + Send + Sync + 'static> LiveService<S> {
    pub(crate) fn bootstrap(
        state: S,
        config: Arc<ReloadConfig>,
        browser_reloader: Option<tower_livereload::Reloader>,
    ) -> Result<Self, HotReloadError> {
        let staged = stage_unique(&config.workspace_root, &config.package)?;
        let gen = load_generation(&staged, state.clone())?;
        let bookkeeping = Arc::new(Bookkeeping {
            generation_counter: AtomicU64::new(1),
            last_success: Mutex::new(Some(Instant::now())),
            last_error: Mutex::new(None),
        });
        Ok(Self {
            state,
            generation: Arc::new(ArcSwap::from_pointee(gen)),
            config,
            bookkeeping,
            browser_reloader,
        })
    }

    pub(crate) async fn reload(&self) -> Result<(), HotReloadError> {
        match self.reload_inner().await {
            Ok(()) => {
                *self.bookkeeping.last_success.lock().unwrap() = Some(Instant::now());
                *self.bookkeeping.last_error.lock().unwrap() = None;
                self.bookkeeping
                    .generation_counter
                    .fetch_add(1, Ordering::Relaxed);
                if let Some(reloader) = &self.browser_reloader {
                    reloader.reload();
                }
                Ok(())
            }
            Err(e) => {
                *self.bookkeeping.last_error.lock().unwrap() = Some(format!("{e}"));
                Err(e)
            }
        }
    }

    async fn reload_inner(&self) -> Result<(), HotReloadError> {
        run_cargo_build(&self.config).await?;
        let staged = stage_unique(&self.config.workspace_root, &self.config.package)?;
        let gen = load_generation(&staged, self.state.clone())?;
        self.generation.store(Arc::new(gen));
        // Best-effort cleanup of staging files no longer referenced.
        sweep_staging(&self.config.workspace_root, &self.config.package);
        Ok(())
    }

    pub(crate) fn status(&self) -> ReloadStatus {
        ReloadStatus {
            generation: self.bookkeeping.generation_counter.load(Ordering::Relaxed),
            last_success: *self.bookkeeping.last_success.lock().unwrap(),
            last_error: self.bookkeeping.last_error.lock().unwrap().clone(),
        }
    }
}

impl<S: Clone + Send + Sync + 'static> Service<Request> for LiveService<S> {
    type Response = Response;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn Future<Output = Result<Response, Infallible>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request) -> Self::Future {
        // Pin the generation for the lifetime of the response. This keeps
        // both the Router and the underlying dylib's text segment alive
        // until the future completes.
        let gen = self.generation.load_full();
        let mut router = gen.router.clone();
        Box::pin(async move {
            let _hold = gen;
            let req = req.map(Body::new);
            router.call(req).await
        })
    }
}

pub(crate) async fn run_cargo_build(config: &ReloadConfig) -> Result<(), HotReloadError> {
    tracing::debug!(package = %config.package, "cargo build");
    let out = tokio::process::Command::new(&config.cargo)
        .arg("build")
        .arg("-p")
        .arg(&config.package)
        .arg("--lib")
        .current_dir(&config.workspace_root)
        .output()
        .await?;
    if !out.status.success() {
        return Err(HotReloadError::Build(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    Ok(())
}

fn load_generation<S: Clone + Send + Sync + 'static>(
    path: &Path,
    state: S,
) -> Result<Generation, HotReloadError> {
    // SAFETY: We trust the user's dylib. The signature contract is documented
    // and the symbol name is namespaced. There is no safe way to do FFI here.
    unsafe {
        let lib = Library::new(path).map_err(HotReloadError::Dlopen)?;
        let build: Symbol<extern "Rust" fn(S) -> Router<()>> =
            lib.get(BUILD_ROUTER_SYMBOL).map_err(|_| {
                HotReloadError::SymbolMissing(
                    std::str::from_utf8(BUILD_ROUTER_SYMBOL)
                        .unwrap()
                        .to_string(),
                )
            })?;
        let router = std::panic::catch_unwind(AssertUnwindSafe(|| build(state)))
            .map_err(|_| HotReloadError::BuildRouterPanic)?;
        // Drop the borrowed Symbol before moving `lib` into the Generation.
        drop(build);
        Ok(Generation { router, _lib: lib })
    }
}

pub(crate) fn built_dylib_path(workspace_root: &Path, package: &str) -> PathBuf {
    let (prefix, ext) = dylib_naming();
    // Cargo emits dylib filenames with hyphens converted to underscores
    // (so package `my-axum-app` produces `libmy_axum_app.dylib`).
    let lib_name = package.replace('-', "_");
    workspace_root
        .join("target/debug")
        .join(format!("{prefix}{lib_name}.{ext}"))
}

fn dylib_naming() -> (&'static str, &'static str) {
    if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else if cfg!(target_os = "windows") {
        ("", "dll")
    } else {
        ("lib", "so")
    }
}

fn staging_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join("target").join("axum-hotreload")
}

fn stage_unique(workspace_root: &Path, package: &str) -> Result<PathBuf, HotReloadError> {
    let src = built_dylib_path(workspace_root, package);
    if !src.exists() {
        return Err(HotReloadError::DylibMissing(src));
    }
    let staging = staging_dir(workspace_root);
    std::fs::create_dir_all(&staging)?;
    let (prefix, ext) = dylib_naming();
    let lib_name = package.replace('-', "_");
    let dst = staging.join(format!(
        "{prefix}{lib_name}-{}.{ext}",
        uuid::Uuid::new_v4()
    ));
    std::fs::copy(&src, &dst)?;
    Ok(dst)
}

/// Best-effort: delete any staged dylib that's no longer referenced.
///
/// We can't track which Arc<Generation> still exists from here, but on
/// most platforms `remove_file` succeeds on a still-mapped file (it just
/// unlinks the dentry; the inode lives until close). On Windows the
/// remove will fail for in-use files — we ignore that and try again later.
fn sweep_staging(workspace_root: &Path, package: &str) {
    let staging = staging_dir(workspace_root);
    let lib_name = package.replace('-', "_");
    let Ok(entries) = std::fs::read_dir(&staging) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.contains(&lib_name))
            .unwrap_or(false)
        {
            continue;
        }
        // Keep files modified in the last 2 seconds — they're likely the
        // current generation. Older files belong to retired generations.
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                if modified.elapsed().unwrap_or_default().as_secs() < 2 {
                    continue;
                }
            }
        }
        let _ = std::fs::remove_file(&path);
    }
}
