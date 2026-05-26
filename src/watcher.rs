use crate::live::LiveService;
use notify::{EventKind, RecursiveMode, Watcher};
use std::{path::PathBuf, time::Duration};

pub(crate) fn spawn<S: Clone + Send + Sync + 'static>(
    live: LiveService<S>,
    watch_dirs: Vec<PathBuf>,
    debounce: Duration,
) -> Result<(), crate::HotReloadError> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            if matches!(
                event.kind,
                EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
            ) {
                let _ = tx.send(());
            }
        }
    })?;

    for dir in &watch_dirs {
        watcher.watch(dir, RecursiveMode::Recursive)?;
        tracing::info!(path = %dir.display(), "watching");
    }

    // Keep the watcher alive for the process lifetime.
    Box::leak(Box::new(watcher));

    tokio::spawn(async move {
        loop {
            if rx.recv().await.is_none() {
                return;
            }
            // Collapse a flurry of events into one rebuild.
            loop {
                match tokio::time::timeout(debounce, rx.recv()).await {
                    Ok(Some(())) => continue,
                    _ => break,
                }
            }
            tracing::info!("change detected, rebuilding");
            match live.reload().await {
                Ok(()) => tracing::info!("swapped"),
                Err(e) => tracing::error!(error = %e, "reload failed"),
            }
        }
    });

    Ok(())
}
