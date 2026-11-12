use crate::{PreparedWorker, WorkerObservation};
use std::future::{pending, Future};
use std::io;
use std::os::fd::OwnedFd;
use std::pin::Pin;

type Observation<'a> =
    Pin<Box<dyn Future<Output = (PreparedWorker<'a>, io::Result<WorkerObservation>)> + Send + 'a>>;

pub(super) enum Worker<'a> {
    Unavailable,
    Prepared(PreparedWorker<'a>),
    Observing(Observation<'a>),
}

impl<'a> Worker<'a> {
    pub fn observe(&mut self) -> io::Result<()> {
        let mut worker = match std::mem::replace(self, Self::Unavailable) {
            Self::Prepared(worker) => worker,
            other => {
                *self = other;
                return Err(io::Error::other("worker is not available for observation"));
            }
        };
        // Cancelling observe invalidates the channel. Keep this future alive
        // while the session handles control replies and lease renewals.
        *self = Self::Observing(Box::pin(async move {
            let observation = worker.observe().await;
            (worker, observation)
        }));
        Ok(())
    }

    pub async fn advance(&mut self) -> io::Result<WorkerObservation> {
        let Self::Observing(observation) = self else {
            return pending().await;
        };
        let (worker, result) = observation.as_mut().await;
        *self = Self::Prepared(worker);
        result
    }

    pub fn verify(&self) -> io::Result<()> {
        match self {
            Self::Prepared(worker) => worker.verify_resources(),
            _ => Err(io::Error::other("worker is not available for Start")),
        }
    }

    pub fn take_terminal(&mut self) -> Option<OwnedFd> {
        match std::mem::replace(self, Self::Unavailable) {
            Self::Prepared(worker) => worker.into_terminal(),
            _ => None,
        }
    }
}
