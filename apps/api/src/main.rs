#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "axial_api=info".into()),
        )
        .init();
    let origin = std::env::var("AXIAL_WEB_ORIGIN").ok();
    let services = match axial_api::start_browser(origin.as_deref()).await {
        Ok(services) => services,
        Err(mut failure) => loop {
            tracing::error!(error = %failure, "startup blocked; preserving profile ownership");
            match failure.try_preserve() {
                Ok(message) => return Err(std::io::Error::other(message).into()),
                Err(retained) => {
                    failure = retained;
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        },
    };
    // The secret capability is available only through native IPC or an exact
    // admitted browser origin; it must never enter logs or command output.
    tracing::info!(base_url = %services.server.bootstrap().base_url, "replacement local API listening");
    let outcome = tokio::select! {
        result = services.server.wait() => result.map_err(std::io::Error::other),
        result = tokio::signal::ctrl_c() => result,
    };
    // Even a listener or signal error must settle owned application effects.
    loop {
        match services.server.shutdown().await {
            Ok(()) => break,
            Err(error) if services.server.is_shutdown_settled() => {
                return Err(std::io::Error::other(error).into());
            }
            Err(error) => {
                tracing::error!(%error, "shutdown remains incomplete; preserving owned work");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    }
    outcome?;
    Ok(())
}
