use crate::driver::Driver;

use std::future::Future;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use tokio::io::unix::AsyncFd;
use tokio::task::LocalSet;

mod context;

pub(crate) use context::RuntimeContext;

thread_local! {
    pub(crate) static CONTEXT: RuntimeContext = RuntimeContext::new();
}

/// The Runtime executor
pub struct Runtime {
    /// io-uring driver
    uring_fd: RawFd,

    /// LocalSet for !Send tasks
    local: LocalSet,

    /// Tokio runtime, always current-thread
    rt: tokio::runtime::Runtime,
}

/// Spawns a new asynchronous task, returning a [`JoinHandle`] for it.
///
/// Spawning a task enables the task to execute concurrently to other tasks.
/// There is no guarantee that a spawned task will execute to completion. When a
/// runtime is shutdown, all outstanding tasks are dropped, regardless of the
/// lifecycle of that task.
///
/// This function must be called from the context of a `tokio-uring` runtime.
///
/// [`JoinHandle`]: tokio::task::JoinHandle
///
/// # Examples
///
/// In this example, a server is started and `spawn` is used to start a new task
/// that processes each received connection.
///
/// ```no_run
/// tokio_uring::start(async {
///     let handle = tokio_uring::spawn(async {
///         println!("hello from a background task");
///     });
///
///     // Let the task complete
///     handle.await.unwrap();
/// });
/// ```
pub fn spawn<T: Future + 'static>(task: T) -> tokio::task::JoinHandle<T::Output> {
    tokio::task::spawn_local(task)
}

impl Runtime {
    /// Create a new tokio_uring runtime on the current thread
    pub fn new(b: &crate::Builder) -> io::Result<Runtime> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .on_thread_park(|| {
                CONTEXT.with(|x| {
                    let _ = x.with_driver_mut(|d| {
                        // optimization: we can potentially avoid an epoll_wait call if we try to
                        // dispatch completions before we park on epoll
                        d.tick();
                        d.submit()
                    });
                });
            })
            .on_thread_unpark(|| {
                CONTEXT.with(|x| {
                    x.with_driver_mut(|d| {
                        // Dispatch completions to wake tasks based on any completed ops.
                        // this is an optimization to try and avoid the whole "give the io driver
                        // a dedicated task" thing
                        d.tick();
                    });
                });
            })
            .enable_all()
            .build()?;

        let local = LocalSet::new();

        let driver = Driver::new(b)?;

        let driver_fd = driver.as_raw_fd();

        CONTEXT.with(|cx| cx.set_driver(driver));

        Ok(Runtime {
            uring_fd: driver_fd,
            local,
            rt,
        })
    }

    /// Runs a future to completion on the current runtime
    pub fn block_on<F>(&self, future: F) -> F::Output
    where
        F: Future,
    {
        let drive = {
            let _guard = self.rt.enter();
            let driver = AsyncFd::new(self.uring_fd).unwrap();

            async move {
                loop {
                    // Wait for read-readiness
                    let mut guard = driver.readable().await.unwrap();
                    CONTEXT.with(|cx| cx.with_driver_mut(|driver| driver.tick()));
                    guard.clear_ready();
                }
            }
        };

        tokio::pin!(future);

        self.local.spawn_local(drive);

        self.rt.block_on(
            self.local
                .run_until(crate::future::poll_fn(|cx| future.as_mut().poll(cx))),
        )
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        CONTEXT.with(|rc| rc.unset_driver())
    }
}
