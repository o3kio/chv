pub mod auth;
pub mod health;
pub mod image;
pub mod migrate;
pub mod network;
pub mod node;
pub mod task;
pub mod user;
pub mod vm;
pub mod volume;

use crate::client::CliError;

/// Issue #656: live/concurrent-write migration is not supported until
/// #394 is proven. Both `vm migrate` and `migrate start` fail closed
/// client-side rather than sending `pause_first=false`; never coerce it
/// to true, which would claim a stop-the-world pause the operator did
/// not request.
pub(crate) fn require_pause_first(pause_first: bool) -> Result<(), CliError> {
    if pause_first {
        Ok(())
    } else {
        Err(CliError::Usage(
            "live/concurrent-write migration is not supported until #394 is proven; \
             pass --pause-first to run a stop-the-world migration"
                .to_string(),
        ))
    }
}
