pub async fn run(bind: &str, port: u16) -> Result<(), String> {
    match super::stop::run().await {
        Ok(()) => {}
        Err(e) if e.contains("not running") => {}
        Err(e) => return Err(e),
    }

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    super::start::run(false, bind, port).await
}
