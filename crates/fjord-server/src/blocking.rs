//! Off the reactor.
//!
//! **fjall is synchronous and the executor is CPU-bound**, so neither belongs on a
//! reactor thread: a query that scans a million rows, or a `create` that materialises
//! every predicate's keyspaces, would stall every other connection the thread happened
//! to be driving. Everything that touches a store goes through here.
//!
//! Its own module because two callers need it — a stream doing a query or an ingest,
//! and the [registry](crate::registry) doing a lifecycle operation — and a helper that
//! lives in whichever of them was written first is a helper the other has to reach
//! backwards for.

use crate::error::ServerError;

/// Run `work` on the blocking pool.
///
/// A panic in the work reaches here as a join error rather than unwinding the
/// connection, so a bug in one query fails that stream instead of taking the server
/// with it.
pub(crate) async fn run<T, F>(work: F) -> Result<T, ServerError>
where
    F: FnOnce() -> Result<T, ServerError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(join) => Err(ServerError::Execution(format!(
            "a blocking task did not finish: {join}"
        ))),
    }
}
