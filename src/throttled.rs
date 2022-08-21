use chrono::Utc;
use tokio::sync::{Semaphore, SemaphorePermit, TryAcquireError};

use lava_api::device::Devices;
use lava_api::job::{self, Jobs, JobsBuilder};
use lava_api::joblog::{JobLog, JobLogBuilder, JobLogRaw};
use lava_api::paginator::{PaginationError, Paginator};
use lava_api::tag::Tag;
use lava_api::worker::Worker;
use lava_api::Lava;

#[derive(Debug)]
pub struct Permit<'a> {
    _permit: SemaphorePermit<'a>,
}

#[derive(Debug)]
pub struct Throttler {
    tickets: Semaphore,
}

impl Throttler {
    pub fn new(max_concurrent_requests: usize) -> Self {
        Self {
            tickets: Semaphore::new(max_concurrent_requests),
        }
    }

    pub async fn acquire(&self) -> Permit<'_> {
        // This unwrap is safe: we never close the semaphore
        let permit = self.tickets.acquire().await.unwrap();
        Permit { _permit: permit }
    }

    #[allow(dead_code)]
    pub(crate) async fn try_acquire(&self) -> Option<Permit<'_>> {
        let permit = match self.tickets.try_acquire() {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => {
                return None;
            }
            Err(TryAcquireError::Closed) => {
                panic!("Semaphore unexpectedly closed in Throttler::try_acquire");
            }
        };

        Some(Permit { _permit: permit })
    }
}

/// A throttled wrapper over a [Lava] instance.
///
/// Each method on this type wraps the method of the same name on the
/// contained Lava instance. In order for the method to progress, it
/// first requests a permit from the contained [Throttler]. Since only
/// so many permits exist, this has the effect of limiting the number
/// of API endpoints that are being queried concurrently.
///
/// This interface has subtleties that can cause problems. In general,
/// you should never seek to obtain another permit whilst you already
/// hold one in a given thread of execution. This can happen in two
/// basic ways:
///
/// - The builders hold a permit, so making a request while using a
///   builder ([Devices], [JobsBuilder] etc) will lead to you trying
///   to hold two permits.
///
/// - The paginated responses hold a permit, so making further queries
///   while looping over paginated data will lead to you trying to
///   obtain two permits.
///
/// The failure case when you obtain multiple permits within a single
/// thread of execution is that you may enter a multi-way deadlock, in
/// which all active tasks are waiting to obtain their second permit,
/// and none is able to release its first permit to allow any other
/// thread to go ahead.
pub struct ThrottledLava {
    inner: Lava,
    throttler: Throttler,
}

#[allow(dead_code)]
impl ThrottledLava {
    pub fn new(inner: Lava, throttler: Throttler) -> ThrottledLava {
        Self { inner, throttler }
    }

    pub async fn refresh_tags(&self) -> Result<(), PaginationError> {
        let _permit = self.throttler.acquire().await;
        self.inner.refresh_tags().await
    }

    pub async fn tag(&self, tag: u32) -> Option<Tag> {
        let _permit = self.throttler.acquire().await;
        self.inner.tag(tag).await
    }

    pub async fn tags(&self) -> Result<Vec<Tag>, PaginationError> {
        let _permit = self.throttler.acquire().await;
        self.inner.tags().await
    }

    pub async fn devices(&self) -> Throttled<Devices> {
        let permit = self.throttler.acquire().await;
        Throttled::new(self.inner.devices(), permit)
    }

    pub async fn log(&self, id: i64) -> ThrottledJobLogBuilder {
        let permit = self.throttler.acquire().await;
        ThrottledJobLogBuilder::new(self.inner.log(id), permit)
    }

    pub async fn jobs(&self) -> ThrottledJobsBuilder {
        let permit = self.throttler.acquire().await;
        ThrottledJobsBuilder::new(self.inner.jobs(), permit)
    }

    pub async fn submit_job(&self, definition: &str) -> Result<Vec<i64>, job::SubmissionError> {
        let _permit = self.throttler.acquire().await;
        job::submit_job(&self.inner, definition).await
    }

    pub async fn workers(&self) -> Throttled<Paginator<Worker>> {
        let permit = self.throttler.acquire().await;
        Throttled::new(self.inner.workers(), permit)
    }
}

pub struct Throttled<'a, T> {
    inner: T,
    _permit: Permit<'a>,
}

impl<'a, T> Throttled<'a, T> {
    pub fn new(inner: T, permit: Permit<'a>) -> Self {
        Self {
            inner,
            _permit: permit,
        }
    }
}

impl<'a, T> core::ops::Deref for Throttled<'a, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<'a, T> core::ops::DerefMut for Throttled<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

pub struct ThrottledJobLogBuilder<'a> {
    inner: JobLogBuilder<'a>,
    _permit: Permit<'a>,
}

#[allow(dead_code)]
impl<'a> ThrottledJobLogBuilder<'a> {
    fn new(inner: JobLogBuilder<'a>, permit: Permit<'a>) -> Self {
        Self {
            inner,
            _permit: permit,
        }
    }

    pub fn start(self, start: u64) -> Self {
        Self {
            inner: self.inner.start(start),
            _permit: self._permit,
        }
    }

    pub fn end(self, end: u64) -> Self {
        Self {
            inner: self.inner.end(end),
            _permit: self._permit,
        }
    }

    pub fn raw(self) -> Throttled<'a, JobLogRaw<'a>> {
        Throttled::new(self.inner.raw(), self._permit)
    }

    pub fn log(self) -> Throttled<'a, JobLog<'a>> {
        Throttled::new(self.inner.log(), self._permit)
    }
}

pub struct ThrottledJobsBuilder<'a> {
    inner: JobsBuilder<'a>,
    permit: Permit<'a>,
}

#[allow(dead_code)]
impl<'a> ThrottledJobsBuilder<'a> {
    fn new(inner: JobsBuilder<'a>, permit: Permit<'a>) -> Self {
        Self { inner, permit }
    }

    pub fn state(self, state: job::State) -> Self {
        Self {
            inner: self.inner.state(state),
            permit: self.permit,
        }
    }

    pub fn state_not(self, state: job::State) -> Self {
        Self {
            inner: self.inner.state_not(state),
            permit: self.permit,
        }
    }

    pub fn limit(self, limit: u32) -> Self {
        Self {
            inner: self.inner.limit(limit),
            permit: self.permit,
        }
    }

    pub fn health(self, health: job::Health) -> Self {
        Self {
            inner: self.inner.health(health),
            permit: self.permit,
        }
    }

    pub fn health_not(self, health: job::Health) -> Self {
        Self {
            inner: self.inner.health_not(health),
            permit: self.permit,
        }
    }

    pub fn id(self, id: i64) -> Self {
        Self {
            inner: self.inner.id(id),
            permit: self.permit,
        }
    }

    pub fn id_after(self, id: i64) -> Self {
        Self {
            inner: self.inner.id_after(id),
            permit: self.permit,
        }
    }

    pub fn started_after(self, when: chrono::DateTime<Utc>) -> Self {
        Self {
            inner: self.inner.started_after(when),
            permit: self.permit,
        }
    }

    pub fn submitted_after(self, when: chrono::DateTime<Utc>) -> Self {
        Self {
            inner: self.inner.submitted_after(when),
            permit: self.permit,
        }
    }

    pub fn ended_after(self, when: chrono::DateTime<Utc>) -> Self {
        Self {
            inner: self.inner.ended_after(when),
            permit: self.permit,
        }
    }

    pub fn ordering(self, ordering: job::Ordering, ascending: bool) -> Self {
        Self {
            inner: self.inner.ordering(ordering, ascending),
            permit: self.permit,
        }
    }

    pub fn query(self) -> Throttled<'a, Jobs<'a>> {
        Throttled::new(self.inner.query(), self.permit)
    }
}

#[cfg(test)]
mod tests {
    use super::Throttler;
    use tokio::test;

    #[test]
    async fn test_throttler() {
        let t = Throttler::new(3);

        let _p1 = t.acquire().await;

        let _p2 = t.acquire().await;

        let p3 = t.acquire().await;

        let no_p4 = t.try_acquire().await;
        assert!(no_p4.is_none());

        drop(p3);

        let _p4 = t.acquire().await;
    }
}
