use crate::convert::invalid;
use oxide_engine::{AutoIdPolicy, Engine, JobPolicy};
use oxide_proto::google::datastore::emulator::config::v1 as pb;
use pb::emulator_config_server::{EmulatorConfig, EmulatorConfigServer};
use std::sync::{Arc, Mutex};
use tonic::{Request, Response, Status};
/// The job-policy part of Java's merged emulator config.
#[derive(Debug, Clone, Copy)]
pub struct StoredJobPolicy {
    pub random_seed: i64,
    pub unapplied_percentage: f32,
    /// Report forced strong consistency (always, in Firestore-in-Datastore mode).
    pub forced: bool,
}
pub struct ConfigService {
    engine: Arc<Engine>,
    stored: Mutex<StoredJobPolicy>,
    firestore: bool,
}
impl ConfigService {
    pub fn new(engine: Arc<Engine>, stored: StoredJobPolicy, firestore: bool) -> Self {
        Self {
            engine,
            stored: Mutex::new(stored),
            firestore,
        }
    }
    fn stored(&self) -> std::sync::MutexGuard<'_, StoredJobPolicy> {
        self.stored.lock().unwrap_or_else(|p| p.into_inner())
    }
    pub fn into_server(self) -> EmulatorConfigServer<Self> {
        EmulatorConfigServer::new(self)
    }
    /// The stored config: the job policy as last merged, and the engine's id policy.
    fn current(&self) -> pb::Config {
        use pb::id_allocation_policy::Policy;
        let stored = *self.stored();
        let policy = if stored.forced {
            pb::job_policy::Policy::ForceStrongConsistency(true)
        } else {
            pb::job_policy::Policy::ProbabilityJobPolicy(pb::ProbabilityJobPolicy {
                random_seed: stored.random_seed,
                unapplied_job_percentage: stored.unapplied_percentage,
            })
        };
        let id_policy = match self.engine.config().auto_id_policy {
            AutoIdPolicy::Sequential => Policy::Sequential,
            AutoIdPolicy::Scattered => Policy::Scattered,
        };
        pb::Config {
            job_policy: Some(pb::JobPolicy {
                policy: Some(policy),
            }),
            id_allocation_policy: Some(pb::IdAllocationPolicy {
                policy: id_policy as i32,
            }),
        }
    }
}

#[tonic::async_trait]
impl EmulatorConfig for ConfigService {
    /// Merges the request into the stored config, then installs forced strong consistency if the
    /// request asks for it, and otherwise the stored probability policy (whatever else the
    /// request holds). Firestore-in-Datastore mode keeps reporting forced strong consistency.
    async fn update_config(
        &self,
        request: Request<pb::UpdateConfigRequest>,
    ) -> Result<Response<pb::Config>, Status> {
        use pb::id_allocation_policy::Policy;
        use pb::job_policy::Policy as Job;
        let config = request.into_inner().config.unwrap_or_default();
        let requested = config.job_policy.and_then(|j| j.policy);
        let stored = {
            let mut stored = self.stored();
            match &requested {
                Some(Job::ProbabilityJobPolicy(p)) => {
                    stored.random_seed = p.random_seed;
                    stored.unapplied_percentage = p.unapplied_job_percentage;
                    stored.forced = self.firestore;
                }
                Some(Job::ForceStrongConsistency(force)) => {
                    stored.forced = self.firestore || *force;
                }
                None => {}
            }
            *stored
        };
        match config.id_allocation_policy.map(|p| p.policy()) {
            Some(Policy::Sequential) => self.engine.set_auto_id_policy(AutoIdPolicy::Sequential),
            Some(Policy::Scattered) => self.engine.set_auto_id_policy(AutoIdPolicy::Scattered),
            _ => {}
        }
        let pct = stored.unapplied_percentage;
        if pct < 0.0 {
            return Err(invalid(format!(
                "Unapplied job percentage must be >= 0 (received {:.6})",
                f64::from(pct)
            )));
        }
        if pct > 100.0 {
            return Err(invalid(format!(
                "Unapplied job percentage must be <= 100.00 (received {:.6})",
                f64::from(pct)
            )));
        }
        self.engine.set_job_policy(
            if matches!(requested, Some(Job::ForceStrongConsistency(true))) {
                JobPolicy::ForceStrongConsistency
            } else {
                JobPolicy::Probability {
                    random_seed: stored.random_seed,
                    unapplied_percentage: pct,
                }
            },
        );
        Ok(Response::new(self.current()))
    }

    async fn get_config(
        &self,
        _: Request<pb::GetConfigRequest>,
    ) -> Result<Response<pb::Config>, Status> {
        Ok(Response::new(self.current()))
    }
}
