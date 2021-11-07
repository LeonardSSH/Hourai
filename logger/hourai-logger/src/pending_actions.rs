use anyhow::Result;
use futures::stream::StreamExt;
use hourai_sql::{Executor, PendingAction};
use hourai_storage::actions::ActionExecutor;
use tokio::time::{Duration, Instant};

const CYCLE_DURATION: Duration = Duration::from_secs(1);

pub async fn run_pending_actions(executor: ActionExecutor) {
    loop {
        let next = Instant::now() + CYCLE_DURATION;
        let mut pending = PendingAction::fetch_expired().fetch(executor.storage().sql());
        while let Some(item) = pending.next().await {
            match item {
                Ok(action) => {
                    tokio::spawn(run_action(executor.clone(), action));
                }
                Err(err) => {
                    tracing::error!("Error while fetching pending actions: {}", err);
                    break;
                }
            }
        }
        tokio::time::sleep_until(next).await;
    }
}

async fn run_action(executor: ActionExecutor, pending: PendingAction) -> Result<()> {
    if let Err(err) = executor.execute_action(pending.action()).await {
        tracing::error!(
            "Error while running pending action ({:?}): {}",
            pending.action(),
            err
        );
    } else {
        executor.storage().sql().execute(pending.delete()).await?;
    }
    Ok(())
}