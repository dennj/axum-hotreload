use anyhow::Result;
use axum::Router;
use example::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_target(true)
        .with_max_level(tracing::Level::INFO)
        .init();

    let state = AppState::default();
    let app = build_app(state).await?;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    tracing::info!("listening on http://{}", listener.local_addr()?);
    if cfg!(debug_assertions) {
        tracing::info!("edit example/src/lib.rs — browser auto-refreshes");
    } else {
        tracing::info!("release mode — handlers statically linked, no hot reload");
    }
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(debug_assertions)]
async fn build_app(state: AppState) -> Result<Router> {
    let hot = axum_hotreload::HotReload::<AppState>::builder()
        .package("example")
        .state(state)
        .build()
        .await?;
    Ok(hot.into_router())
}

#[cfg(not(debug_assertions))]
async fn build_app(state: AppState) -> Result<Router> {
    Ok(example::axum_hotreload_build_router(state))
}
