use super::*;

#[tokio::test(start_paused = true)]
async fn ended_event_stream_waits_before_the_next_full_refresh() {
    let wait = tokio::spawn(wait_before_catalog_reconnect(false));
    tokio::task::yield_now().await;
    assert!(!wait.is_finished());

    tokio::time::advance(global::FULL_REFRESH_INTERVAL).await;
    wait.await.unwrap();
}
