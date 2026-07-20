//! Write-behind audit writer (ADR-0001): drains the domain audit channel into
//! Postgres. Fail-fast at boot, retry-forever at runtime, drain on shutdown —
//! evidence is never dropped by design, only by process death.

use {
    crate::domain::audit::AuditEvent,
    anyhow::Context,
    sqlx::postgres::{PgPool, PgPoolOptions},
    std::time::Duration,
    tokio::{sync::mpsc, task::JoinHandle},
};

/// Connect and run migrations. Called at boot; an unreachable database is
/// fatal — the service must never run without its evidence trail.
pub async fn connect_and_migrate(database_url: &str) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .context("connecting to audit database")?;
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("running audit database migrations")?;
    Ok(pool)
}

/// Highest proposal ID ever recorded, or 0 on a fresh trail. The audit trail
/// is the ID authority: the in-memory counter reseeds from here at boot so
/// IDs stay unique across restarts and evidence rows stay unambiguous.
pub async fn max_proposal_id(pool: &PgPool) -> anyhow::Result<crate::domain::proposal::ProposalId> {
    let max: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(proposal_id), 0) FROM audit_events")
        .fetch_one(pool)
        .await
        .context("reading max proposal id from audit trail")?;
    let id = u64::try_from(max).context("negative proposal id in audit trail")?;
    Ok(crate::domain::proposal::ProposalId(id))
}

const INITIAL_BACKOFF: Duration = Duration::from_millis(100);
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// Spawn the writer task. It exits only when every sender is dropped *and*
/// the channel is drained, so awaiting the handle after the server stops is
/// the shutdown flush.
pub fn spawn(pool: PgPool, mut rx: mpsc::UnboundedReceiver<AuditEvent>) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            insert_with_retry(&pool, &event, rx.len()).await;
        }
        tracing::debug!("audit channel closed and drained; writer exiting");
    })
}

/// Insert one event, retrying with capped exponential backoff until it lands.
/// Events queue in the unbounded channel meanwhile; a long outage is an ops
/// page, not a reason to drop evidence.
async fn insert_with_retry(pool: &PgPool, event: &AuditEvent, queued: usize) {
    let mut backoff = INITIAL_BACKOFF;
    loop {
        match insert(pool, event).await {
            Ok(()) => return,
            Err(err) => {
                tracing::error!(
                    %err,
                    proposal_id = %event.proposal_id(),
                    event_type = event.event_type(),
                    queued,
                    backoff_ms = backoff.as_millis() as u64,
                    "audit insert failed; retrying"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

async fn insert(pool: &PgPool, event: &AuditEvent) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO audit_events (proposal_id, event_type, sub_solver, order_uid, \
         settlement_tx_hash, payload, occurred_at) VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(i64::try_from(event.proposal_id().0).expect("proposal id exceeds i64"))
    .bind(event.event_type())
    .bind(format!("{:#x}", event.sub_solver()))
    .bind(event.order_uid().to_string())
    // No settling event types exist yet; reserved for ADR-0010 outcomes.
    .bind(Option::<String>::None)
    .bind(event.payload())
    .bind(chrono::DateTime::<chrono::Utc>::from(event.occurred_at))
    .execute(pool)
    .await
    .map(|_| ())
}
