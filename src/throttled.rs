use chrono::Utc;
use tokio::sync::{Semaphore, SemaphorePermit, TryAcquireError};

use lava_api::device::Devices;
use lava_api::job::{self, Jobs, JobsBuilder};
use lava_api::joblog::{JobLog, JobLogBuilder, JobLogRaw};
use lava_api::paginator::{PaginationError, Paginator};
use lava_api::tag::Tag;
use lava_api::worker::Worker;
use lava_api::Lava;

#[cfg(debug_assertions)]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(debug_assertions)]
use std::sync::Mutex;
#[cfg(debug_assertions)]
use std::time::Instant;
#[cfg(debug_assertions)]
use tracing::{debug, trace};

#[cfg(debug_assertions)]
#[derive(Debug)]
struct PermitDebug<'a> {
    owner: &'a ThrottlerDebug,
    debug_name: String,
    start: Option<Instant>,
}

#[cfg(debug_assertions)]
impl<'a> Drop for PermitDebug<'a> {
    fn drop(&mut self) {
        match self.start {
            Some(start) => {
                let now = Instant::now();
                debug!(
                    "Permit released for {} (held for {}s)",
                    self.debug_name,
                    now.duration_since(start).as_secs_f32()
                );
                let mut g = self.owner.active_permits.lock().unwrap();
                let ix = g.iter().position(|x| *x == self.debug_name).unwrap();
                g.remove(ix);
            }
            None => {
                debug!(
                    "Permit debug data dropped before permit was acquired for {}",
                    self.debug_name
                );
            }
        }
    }
}

#[cfg(debug_assertions)]
#[derive(Debug)]
struct ThrottlerDebug {
    active_permits: Mutex<Vec<String>>,
    next_id: AtomicUsize,
}

#[cfg(debug_assertions)]
impl ThrottlerDebug {
    pub fn new() -> Self {
        Self {
            active_permits: Mutex::new(Vec::new()),
            next_id: AtomicUsize::new(0),
        }
    }

    pub fn pre_acquire(&self, name: &str, throttler: &Throttler) -> PermitDebug<'_> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let target = format!("{} ({})", name, id);

        debug!(
            "Waiting for permit {} (available {})",
            target,
            throttler.tickets.available_permits()
        );

        {
            let g = self.active_permits.lock().unwrap();
            for job in g.iter() {
                trace!("Active job: {}", job);
            }
        }

        PermitDebug {
            owner: self,
            debug_name: target,
            start: None,
        }
    }

    pub fn post_acquire<'a>(&self, mut permit_debug: PermitDebug<'a>) -> PermitDebug<'a> {
        self.active_permits
            .lock()
            .unwrap()
            .push(permit_debug.debug_name.clone());
        debug!("Permit acquired {}", permit_debug.debug_name);
        permit_debug.start = Some(Instant::now());
        permit_debug
    }
}

#[derive(Debug)]
pub struct Permit<'a> {
    _permit: SemaphorePermit<'a>,
    #[cfg(debug_assertions)]
    _debug: PermitDebug<'a>,
}

#[derive(Debug)]
pub struct Throttler {
    tickets: Semaphore,
    #[cfg(debug_assertions)]
    debug: ThrottlerDebug,
}

impl Throttler {
    pub fn new(max_concurrent_requests: usize) -> Self {
        Self {
            tickets: Semaphore::new(max_concurrent_requests),
            #[cfg(debug_assertions)]
            debug: ThrottlerDebug::new(),
        }
    }

    pub async fn acquire(&self, _debug: &str) -> Permit<'_> {
        #[cfg(debug_assertions)]
        let debug = self.debug.pre_acquire(_debug, self);

        // This unwrap is safe: we never close the semaphore
        let permit = self.tickets.acquire().await.unwrap();

        #[cfg(debug_assertions)]
        let debug = self.debug.post_acquire(debug);

        Permit {
            _permit: permit,
            #[cfg(debug_assertions)]
            _debug: debug,
        }
    }

    #[allow(dead_code)]
    pub(crate) async fn try_acquire(&self, _debug: &str) -> Option<Permit<'_>> {
        #[cfg(debug_assertions)]
        let debug = self.debug.pre_acquire(_debug, self);

        let permit = match self.tickets.try_acquire() {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => {
                return None;
            }
            Err(TryAcquireError::Closed) => {
                panic!("Semaphore unexpectedly closed in Throttler::try_acquire");
            }
        };

        #[cfg(debug_assertions)]
        let debug = self.debug.post_acquire(debug);

        Some(Permit {
            _permit: permit,
            #[cfg(debug_assertions)]
            _debug: debug,
        })
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
        let _permit = self.throttler.acquire("refresh_tags").await;
        self.inner.refresh_tags().await
    }

    pub async fn tag(&self, tag: u32) -> Option<Tag> {
        let _permit = self.throttler.acquire("tag").await;
        self.inner.tag(tag).await
    }

    pub async fn tags(&self) -> Result<Vec<Tag>, PaginationError> {
        let _permit = self.throttler.acquire("tags").await;
        self.inner.tags().await
    }

    pub async fn devices(&self) -> Throttled<Devices> {
        let permit = self.throttler.acquire("devices").await;
        Throttled::new(self.inner.devices(), permit)
    }

    pub async fn log(&self, id: i64) -> ThrottledJobLogBuilder {
        let permit = self.throttler.acquire("log").await;
        ThrottledJobLogBuilder::new(self.inner.log(id), permit)
    }

    pub async fn jobs(&self) -> ThrottledJobsBuilder {
        let permit = self.throttler.acquire("jobs").await;
        ThrottledJobsBuilder::new(self.inner.jobs(), permit)
    }

    pub async fn submit_job(&self, definition: &str) -> Result<Vec<i64>, job::SubmissionError> {
        let _permit = self.throttler.acquire("submit_job").await;
        job::submit_job(&self.inner, definition).await
    }

    pub async fn cancel_job(&self, job: i64) -> Result<(), job::CancellationError> {
        let _permit = self.throttler.acquire("cancel_job").await;
        job::cancel_job(&self.inner, job).await
    }

    pub async fn workers(&self) -> Throttled<Paginator<Worker>> {
        let permit = self.throttler.acquire("workers").await;
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

        let _p1 = t.acquire("p1").await;

        let _p2 = t.acquire("p2").await;

        let p3 = t.acquire("p3").await;

        let no_p4 = t.try_acquire("p4 first").await;
        assert!(no_p4.is_none());

        drop(p3);

        let _p4 = t.acquire("p4").await;
    }
}
