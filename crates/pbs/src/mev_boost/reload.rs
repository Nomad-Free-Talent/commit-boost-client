use cb_common::config::load_pbs_config;
use tracing::warn;

use crate::{BuilderApiState, PbsState, PbsStateGuard};

/// Reload the PBS state with the latest configuration in the config file
/// Returns 200 if successful or 500 if failed
pub async fn reload<S: BuilderApiState>(state: PbsStateGuard<S>) -> eyre::Result<()> {
    let prev_state = state.read().await;

    let pbs_config = load_pbs_config().await?;
    let new_state = PbsState::new(pbs_config).with_data(prev_state.data.clone());

    if prev_state.config.pbs_config.host != new_state.config.pbs_config.host {
        warn!(
            "Host change for PBS module require a full restart. Old: {}, New: {}",
            prev_state.config.pbs_config.host, new_state.config.pbs_config.host
        );
    }

    if prev_state.config.pbs_config.port != new_state.config.pbs_config.port {
        warn!(
            "Port change for PBS module require a full restart. Old: {}, New: {}",
            prev_state.config.pbs_config.port, new_state.config.pbs_config.port
        );
    }

    *state.write().await = new_state;

    Ok(())
}
