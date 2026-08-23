//! The one stop-signal future both the foreground `drums watch` binary
//! (`main.rs`) and the `drumsd` daemon binary wait on. Moved here verbatim
//! out of `main.rs` (no behavior change) so `drumsd` can share it instead of
//! re-registering its own signal handler — `tokio::signal` installs an OS
//! handler process-wide, once, permanently, so a SECOND definition living in
//! a second binary is fine (different processes), but a duplicated
//! definition sitting in two source files in the SAME crate is exactly the
//! kind of pipeline-adjacent logic the daemon work was told to share rather
//! than fork.

/// Resolves the first time the process receives ctrl-c OR (on Unix) SIGTERM.
/// A failure to register either handler parks that arm forever rather than
/// resolving immediately — firing on a registration error would tear the
/// watch down instantly for a reason the human never asked for, and would
/// make `kill -9` needed even for the signal that DID register.
pub async fn wait_for_stop_signal() {
    #[cfg(unix)]
    {
        let ctrl_c = async {
            if tokio::signal::ctrl_c().await.is_err() {
                std::future::pending::<()>().await
            }
        };
        let sigterm = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut sig) => {
                    sig.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm => {}
        }
    }
    #[cfg(not(unix))]
    {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await
        }
    }
}
