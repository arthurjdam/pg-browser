//! Bridge between GPUI's executor and tokio. Database work runs on a small tokio runtime; the UI
//! awaits the join handle from GPUI tasks, so the UI thread never blocks on I/O.

use pgcore::UserFacingError;
use std::future::Future;
use std::sync::OnceLock;
use tokio::runtime::{Builder, Runtime};

fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("pgb-db")
            .enable_all()
            .build()
            .expect("failed to start the database runtime")
    })
}

/// Runs a fallible database future on the tokio runtime. A panic inside the task is reported as an
/// error instead of unwinding into the UI.
pub async fn run<T, F>(fut: F) -> Result<T, UserFacingError>
where
    F: Future<Output = Result<T, UserFacingError>> + Send + 'static,
    T: Send + 'static,
{
    match runtime().spawn(fut).await {
        Ok(result) => result,
        Err(join_err) => Err(UserFacingError::config(
            "Internal error",
            format!("A background database task failed unexpectedly: {join_err}"),
        )),
    }
}
