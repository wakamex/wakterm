use anyhow::{anyhow, Result};
use async_executor::Executor;
use flume::{bounded, unbounded, Receiver, RecvTimeoutError};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub use async_task::{Runnable, Task};
pub type SpawnFunc = Box<dyn FnOnce() + Send>;
pub type ScheduleFunc = Box<dyn Fn(Runnable) + Send + Sync + 'static>;

fn no_scheduler_configured(_: Runnable) {
    panic!("no scheduler has been configured");
}

lazy_static::lazy_static! {
    static ref ON_MAIN_THREAD: Mutex<ScheduleFunc> = Mutex::new(Box::new(no_scheduler_configured));
    static ref ON_MAIN_THREAD_LOW_PRI: Mutex<ScheduleFunc> = Mutex::new(Box::new(no_scheduler_configured));
    static ref SCOPED_EXECUTOR: Mutex<Option<Arc<Executor<'static>>>> = Mutex::new(None);
}

static SCHEDULER_CONFIGURED: AtomicBool = AtomicBool::new(false);

fn schedule_runnable(runnable: Runnable, high_pri: bool) {
    let func = if high_pri {
        ON_MAIN_THREAD.lock()
    } else {
        ON_MAIN_THREAD_LOW_PRI.lock()
    }
    .unwrap();
    func(runnable);
}

pub fn is_scheduler_configured() -> bool {
    SCHEDULER_CONFIGURED.load(Ordering::Relaxed)
}

/// Set callbacks for scheduling normal and low priority futures.
/// Why this and not "just tokio"?  In a GUI application there is typically
/// a special GUI processing loop that may need to run on the "main thread",
/// so we can't just run a tokio/mio loop in that context.
/// This particular crate has no real knowledge of how that plumbing works,
/// it just provides the abstraction for scheduling the work.
/// This function allows the embedding application to set that up.
pub fn set_schedulers(main: ScheduleFunc, low_pri: ScheduleFunc) {
    *ON_MAIN_THREAD.lock().unwrap() = Box::new(main);
    *ON_MAIN_THREAD_LOW_PRI.lock().unwrap() = Box::new(low_pri);
    SCHEDULER_CONFIGURED.store(true, Ordering::Relaxed);
}

/// Spawn a new thread to execute the provided function.
/// Returns a JoinHandle that implements the Future trait
/// and that can be used to await and yield the return value
/// from the thread.
/// Can be called from any thread.
pub fn spawn_into_new_thread<F, T>(f: F) -> Task<Result<T>>
where
    F: FnOnce() -> Result<T>,
    F: Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    // The channel wakes the receiver when the result is sent, and also when
    // the thread ends without sending one, as when it panics.
    spawn_into_main_thread(async move {
        rx.recv_async()
            .await
            .map_err(|_| anyhow!("thread terminated without providing a result"))?
    })
}

fn get_scoped() -> Option<Arc<Executor<'static>>> {
    SCOPED_EXECUTOR.lock().unwrap().as_ref().map(Arc::clone)
}

/// Spawn a future into the main thread; it will be polled in the
/// main thread.
/// This function can be called from any thread.
/// If you are on the main thread already, consider using
/// spawn() instead to lift the `Send` requirement.
pub fn spawn_into_main_thread<F, R>(future: F) -> Task<R>
where
    F: Future<Output = R> + Send + 'static,
    R: Send + 'static,
{
    if let Some(executor) = get_scoped() {
        return executor.spawn(future);
    }
    let (runnable, task) = async_task::spawn(future, |runnable| schedule_runnable(runnable, true));
    runnable.schedule();
    task
}

/// Spawn a future into the main thread; it will be polled in
/// the main thread in the low priority queue--all other normal
/// priority items will be drained before considering low priority
/// spawns.
/// If you are on the main thread already, consider using `spawn_with_low_priority`
/// instead to lift the `Send` requirement.
pub fn spawn_into_main_thread_with_low_priority<F, R>(future: F) -> Task<R>
where
    F: Future<Output = R> + Send + 'static,
    R: Send + 'static,
{
    if let Some(executor) = get_scoped() {
        return executor.spawn(future);
    }
    let (runnable, task) = async_task::spawn(future, |runnable| schedule_runnable(runnable, false));
    runnable.schedule();
    task
}

/// Spawn a future with normal priority.
pub fn spawn<F, R>(future: F) -> Task<R>
where
    F: Future<Output = R> + 'static,
    R: 'static,
{
    let (runnable, task) =
        async_task::spawn_local(future, |runnable| schedule_runnable(runnable, true));
    runnable.schedule();
    task
}

/// Spawn a future with low priority; it will be polled only after
/// all other normal priority items are processed.
pub fn spawn_with_low_priority<F, R>(future: F) -> Task<R>
where
    F: Future<Output = R> + 'static,
    R: 'static,
{
    let (runnable, task) =
        async_task::spawn_local(future, |runnable| schedule_runnable(runnable, false));
    runnable.schedule();
    task
}

/// Block the current thread until the passed future completes.
pub use async_io::block_on;

pub struct SimpleExecutor {
    rx: Receiver<SpawnFunc>,
}

impl SimpleExecutor {
    pub fn new() -> Self {
        let (tx, rx) = unbounded();

        let tx_main = tx.clone();
        let tx_low = tx.clone();
        let queue_func = move |f: SpawnFunc| {
            tx_main.send(f).ok();
        };
        let queue_func_low = move |f: SpawnFunc| {
            tx_low.send(f).ok();
        };
        set_schedulers(
            Box::new(move |task| {
                queue_func(Box::new(move || {
                    task.run();
                }))
            }),
            Box::new(move |task| {
                queue_func_low(Box::new(move || {
                    task.run();
                }))
            }),
        );
        Self { rx }
    }

    pub fn tick(&self) -> anyhow::Result<()> {
        match self.rx.recv() {
            Ok(func) => func(),
            Err(err) => anyhow::bail!("while waiting for events: {:?}", err),
        };
        Ok(())
    }

    pub fn tick_timeout(&self, timeout: Duration) -> anyhow::Result<bool> {
        match self.rx.recv_timeout(timeout) {
            Ok(func) => {
                func();
                Ok(true)
            }
            Err(RecvTimeoutError::Timeout) => Ok(false),
            Err(err) => anyhow::bail!("while waiting for events: {:?}", err),
        }
    }
}

pub struct ScopedExecutor {}

impl ScopedExecutor {
    pub fn new() -> Self {
        SCOPED_EXECUTOR
            .lock()
            .unwrap()
            .replace(Arc::new(Executor::new()));

        Self {}
    }

    pub async fn run<T>(&self, future: impl Future<Output = T>) -> T {
        get_scoped()
            .expect("SCOPED_EXECUTOR to be alive as long as ScopedExecutor")
            .run(future)
            .await
    }
}

impl Drop for ScopedExecutor {
    fn drop(&mut self) {
        SCOPED_EXECUTOR.lock().unwrap().take();
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A thread that finishes while its result is being polled for must
    /// still wake the task waiting for it.
    #[test]
    fn every_thread_result_wakes_its_waiting_task() {
        const THREADS: usize = 20_000;
        let executor = SimpleExecutor::new();
        let finished = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&finished);
        spawn_into_main_thread(async move {
            for i in 0..THREADS {
                assert_eq!(spawn_into_new_thread(move || Ok(i)).await.unwrap(), i);
                counter.fetch_add(1, Ordering::Relaxed);
            }
        })
        .detach();
        while finished.load(Ordering::Relaxed) < THREADS {
            assert!(
                executor.tick_timeout(Duration::from_secs(5)).unwrap(),
                "a thread's result did not wake its task after {} results",
                finished.load(Ordering::Relaxed)
            );
        }

        // A thread that panics ends its task with an error. Both checks
        // share one test, since each executor replaces the global scheduler.
        let task = spawn_into_new_thread(|| -> Result<()> { panic!("expected") });
        let result = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&result);
        spawn_into_main_thread(async move {
            *slot.lock().unwrap() = Some(task.await);
        })
        .detach();
        while result.lock().unwrap().is_none() {
            assert!(executor.tick_timeout(Duration::from_secs(5)).unwrap());
        }
        assert!(result.lock().unwrap().take().unwrap().is_err());
    }
}
