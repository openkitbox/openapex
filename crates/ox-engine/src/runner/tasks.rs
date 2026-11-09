//! Runner task registration endpoints. Supervisors own handles independently
//! of actors and callers waiting for shutdown.

use ox_tools::contract::AdmissionGate;
use ox_tools::contract::TaskOwner;

#[derive(Clone, Default)]
pub(super) struct Tasks {
    pub gate: AdmissionGate,
    pub actors: TaskOwner,
    pub auxiliary: TaskOwner,
}

impl Tasks {
    pub fn spawn_auxiliary(
        &self,
        future: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> anyhow::Result<()> {
        self.gate.register(|| self.auxiliary.spawn(future))??;
        Ok(())
    }
}
