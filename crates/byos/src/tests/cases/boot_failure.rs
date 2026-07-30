//! Boot failures must still tear down: the background loops stop and the
//! audit writer drains before the error surfaces.

use {
    crate::tests::setup::TestDb,
    std::time::Duration,
    tokio::{net::TcpListener, sync::oneshot},
};

/// A bind failure leaves no background task behind.
///
/// `api::serve` used to be `?`-ed, which skipped the teardown below it: the
/// validation, retention, and penalty loops kept polling the database with
/// nobody left to abort them. Counting live runtime tasks is what makes that
/// observable — the error return alone looks identical either way, which is
/// why an earlier version of this test passed with the bug reintroduced.
#[ignore]
#[tokio::test]
async fn a_bind_failure_leaves_no_background_tasks_running() {
    let db = TestDb::create().await;
    // Hold the port the service will be told to use.
    let occupied = TcpListener::bind("127.0.0.1:0").await.expect("bind probe");
    let taken = occupied.local_addr().expect("probe addr");

    let args = [
        "byos",
        "--public-addr",
        &taken.to_string(),
        "--internal-addr",
        "127.0.0.1:0",
        "--chain-id",
        "1",
        "--trampoline-factory",
        "0x0000000000000000000000000000000000000001",
        "--database-url",
        &db.url,
        "--validation-interval-secs",
        "3600",
    ]
    .into_iter()
    .map(String::from)
    .collect::<Vec<_>>();

    let metrics = tokio::runtime::Handle::current().metrics();
    let baseline = metrics.num_alive_tasks();

    let (bind_tx, _bind_rx) = oneshot::channel();
    let (_shutdown_tx, shutdown_rx) = oneshot::channel();

    // Teardown awaits the audit writer, so a regression that leaves a loop
    // holding an audit sender deadlocks rather than failing an assertion.
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        crate::run_until(args, bind_tx, shutdown_rx),
    )
    .await
    .expect("boot failure must not hang: teardown never completed");

    assert!(
        result.is_err(),
        "binding an occupied port must surface as an error"
    );

    // Aborts are asynchronous: the task is cancelled but the runtime reclaims
    // it on its own schedule, so poll to a deadline rather than sampling once.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let alive = metrics.num_alive_tasks();
        if alive <= baseline {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{} task(s) outlived the failed boot (baseline {baseline}, now {alive}) — a \
             background loop was never aborted",
            alive - baseline
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
