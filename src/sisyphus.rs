use std::{
    future::{Future, IntoFuture},
    panic,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
};

use futures::FutureExt;
use tokio::{
    select,
    sync::{oneshot, watch},
    task::JoinHandle,
};
use tracing::Instrument;

use crate::utils;

/// An error when pushing a `Boulder`
///
/// ## Recoverability
///
/// [`Boulder`]s explicitly mark themselves as `Recoverable` or `Unrecoverable`
/// and further mark unrecoverable errors as `exceptional`, and no outside
/// runner is required to guess or attempt to handle errors.
///
/// ## Tracing
///
/// Recoverable errors will be traced at `DEBUG`. These are considered normal
/// program execution, and indicate temporary failures like a rate-limit
///
/// Exceptional unrecoverable errors will be traced at `ERROR` level, while
/// unexceptional errors will be traced at `TRACE`. Unexceptional errors are
/// typically program lifecycle events. E.g. a task cancellation, shutdown
/// signal, upstream or downstream pipe failure (indicating another task has
/// permanently dropped its pipe), &c.
#[derive(Debug)]
pub enum Fall<T> {
    /// A recoverable issue
    Recoverable {
        /// The task that triggered the issue, for re-spawning
        task: T,
        /// The issue that triggered the fall
        err: eyre::Report,
        /// The shutdown channel, for gracefully shutting down the task
        shutdown: ShutdownSignal,
    },
    /// An unrecoverable issue
    Unrecoverable {
        /// Whether it should be considered exceptional.
        exceptional: bool,
        /// The issue that triggered the fall
        err: eyre::Report,
        /// The task that triggered the issue
        task: T,
    },
    /// The signal for shutting down the task has been sent
    Shutdown {
        /// The task that triggered the issue
        task: T,
    },
}

/// The current state of a Sisyphus task.
#[derive(Debug, Clone)]
pub enum TaskStatus {
    /// Task is starting
    Starting,
    /// Task is running
    Running,
    /// Task is waiting to resume running
    Recovering(Arc<eyre::Report>),
    /// Task is stopped, and will not resume
    Stopped {
        /// Whether the error is exceptional, or normal lifecycle
        exceptional: bool,
        /// The error that triggered the stop
        err: Arc<eyre::Report>,
    },
    /// Task has panicked
    Panicked,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskStatus::Starting => write!(f, "Starting"),
            TaskStatus::Running => write!(f, "Running"),
            TaskStatus::Recovering(e) => write!(f, "Restarting:\n{e}"),
            TaskStatus::Stopped { exceptional, err } => write!(
                f,
                "Stopped:\n{}{}",
                if *exceptional { "exceptional\n" } else { "" },
                err,
            ),
            TaskStatus::Panicked => write!(f, "Panicked"),
        }
    }
}

/// A wrapper around a task that should run forever.
///
/// It exposes an interface for gracefully shutting down the task, as well as
/// inspecting the task's state. Sisyphus tasks do NOT produce an output. If you
/// would like to extract data from the task, make sure that your `Boulder`
/// includes a channel
///
/// ### Lifecycle
///
/// Sisyphus tasks follow a simple lifecycle:
/// - Before the task has commenced work it is `Starting`. At that state, the [`Boulder::bootstrap`] function is called
/// - Once work has commenced it is `Running`
/// - If work was interrupted it goes to 1 of 3 states:
///     - `Recovering(eyre::Report)` - indeicates that the task encountered a
///       recoverable error, and will resume running shortly. At that state, the [`Boulder::recover`] function is called
///     - `Stopped(eyre::Report)` - indicates that the task encountered an
///       unrecoverable will not resume running
///     - `Panicked` - indicates that the task has panicked, and will not resume
///       running
/// - If the shutdown signal is received, it goes into `Stopped`. At that state, the
/// [`Boulder::cleanup`] function is called
///
/// ### Why `eyre::Report`? Why not an associated `Error` type?
///
/// A [`Boulder`] is opaque to the environment relying on it. Its lifecycle
/// should be managed by its internal crash+recovery loop. Associated error
/// types add significant complexity to the management system (e.g. adding an
/// error output would require a generic trait bound as follows:
/// `Sisyphus<T: Boulder> { _phantom: PhantomData<T>}`
///
/// To avoid code complexity AND prevent developers from interfering in the
/// lifecycle of the task, we do not allow easy error handling. In other words,
/// errors are intended to be either ignored or traced, never handled. Because
/// its errors are not intended to be handled, we do not expose them to the
/// outside world.
pub struct Sisyphus {
    pub(crate) restarts: Arc<AtomicUsize>,
    pub(crate) status: tokio::sync::watch::Receiver<TaskStatus>,
    pub(crate) shutdown: tokio::sync::oneshot::Sender<()>,
    pub(crate) task: JoinHandle<()>,
}

impl Sisyphus {
    /// Issue a shutdown command to the task.
    ///
    /// This sends a shutdown command to the relevant task.
    ///
    /// ### Returns
    ///
    /// The `JoinHandle` to the task, so it can be awaited (if necessary).
    pub fn shutdown(self) -> JoinHandle<()> {
        let _ = self.shutdown.send(());
        self.task
    }

    /// Wait for the task status to change. Returns the new status if it has changed,  
    /// or an error if the status channel is closed. It returns immediately if the current
    /// status has not be read, else it wait to change
    ///
    /// # Returns
    ///
    /// - `Ok(TaskStatus)`: The new task status if it has changed.
    /// - `Err(watch::error::RecvError)`: An error indicating that the status channel is closed.
    ///
    /// # Examples
    ///
    /// ```
    /// use sisyphus-tasks::sisyphus::TaskStatus;
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    ///
    /// let mut task = Task::new();
    /// let status = task.watch_status().await;
    ///
    /// match status {
    ///     Ok(new_status) => {
    ///         println!("New status: {:?}", new_status);
    ///     },
    ///     Err(err) => {
    ///         println!("Error: {:?}", err);
    ///     }
    /// }
    ///
    /// # }
    /// ```
    pub async fn watch_status(&mut self) -> Result<TaskStatus, watch::error::RecvError> {
        self.status.changed().await?;
        Ok(self.status.borrow().clone())
    }

    /// Return the task's current status
    pub fn status(&self) -> TaskStatus {
        self.status.borrow().clone()
    }

    /// The number of times the task has restarted
    pub fn restarts(&self) -> usize {
        self.restarts.load(Ordering::Relaxed)
    }
}

impl IntoFuture for Sisyphus {
    type Output = <JoinHandle<()> as IntoFuture>::Output;

    type IntoFuture = <JoinHandle<()> as IntoFuture>::IntoFuture;

    fn into_future(self) -> Self::IntoFuture {
        self.task
    }
}

#[derive(Debug)]
/// The shutdown signal for the task
pub struct ShutdownSignal(oneshot::Receiver<()>);

impl Future for ShutdownSignal {
    type Output = Result<(), oneshot::error::RecvError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.0.poll_unpin(cx) {
            Poll::Ready(res) => Poll::Ready(res),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Convenience trait for conerting errors to [`Fall`]
pub trait ErrExt: std::error::Error + Sized + Send + Sync + 'static {
    /// Convert an error to a recoverable [`Fall`]
    fn recoverable<Task>(self, task: Task, shutdown: ShutdownSignal) -> Fall<Task>
    where
        Task: Boulder,
    {
        Fall::Recoverable {
            task,
            shutdown,
            err: eyre::eyre!(self),
        }
    }

    /// Convert an error to an unrecoverable [`Fall`]
    fn unrecoverable<Task>(self, task: Task, exceptional: bool) -> Fall<Task>
    where
        Task: Boulder,
    {
        Fall::Unrecoverable {
            exceptional,
            err: eyre::eyre!(self),
            task,
        }
    }

    /// Convert an error to an exceptional, unrecoverable [`Fall`]
    fn log_unrecoverable<Task>(self, task: Task) -> Fall<Task>
    where
        Task: Boulder,
    {
        self.unrecoverable(task, true)
    }

    /// Convert an error to an unexcpetional, unrecoverable [`Fall`]
    fn silent_unrecoverable<Task>(self, task: Task) -> Fall<Task>
    where
        Task: Boulder,
    {
        self.unrecoverable(task, false)
    }
}

impl<T> ErrExt for T where T: std::error::Error + Send + Sync + 'static {}

/// A looping, fallible task
pub trait Boulder: std::fmt::Display + Sized {
    /// Defaults to 15 seconds. Can be overridden with arbitrary behavior
    fn restart_after_ms(&self) -> u64 {
        15_000
    }

    /// A short description of the task, defaults to Display impl
    fn task_description(&self) -> String {
        format!("{self}")
    }

    /// Perform the task
    fn spawn(self, shutdown: ShutdownSignal) -> JoinHandle<Fall<Self>>
    where
        Self: 'static + Send + Sync + Sized;

    /// Clean up the task state. This method will be called by the loop when
    /// the task is shutting down due to an unrecoverable error
    ///
    /// Override this function if your task needs to clean up resources on
    /// an unrecoverable error
    fn cleanup(self) -> Pin<Box<dyn Future<Output = eyre::Result<()>> + Send>>
    where
        Self: 'static + Send + Sync + Sized,
    {
        Box::pin(async move { Ok(()) })
    }

    /// Perform any work required to reboot the task. This method will be
    /// called by the loop when the task has encountered a recoverable error.
    ///
    /// Override this function if your task needs to adjust its state when
    /// hitting a recoverable error
    fn recover(self) -> Pin<Box<dyn Future<Output = eyre::Result<Self>> + Send>>
    where
        Self: 'static + Send + Sync + Sized,
    {
        Box::pin(async move { Ok(self) })
    }
    /// Run the task until it panics. Errors result in a task restart with the
    /// same channels. This means that an error causes the task to lose only
    /// the data that is in-scope when it faults.
    fn run_until_panic(self) -> Sisyphus
    where
        Self: 'static + Send + Sync + Sized,
    {
        let task_description = self.task_description();

        let (tx, rx) = watch::channel(TaskStatus::Starting);
        let (shutdown_tx, shutdown_recv) = oneshot::channel();
        let shutdown = ShutdownSignal(shutdown_recv);
        let restarts: Arc<AtomicUsize> = Default::default();
        let restarts_loop_ref = restarts.clone();
        let task: JoinHandle<()> = tokio::spawn(async move {
            let handle = self.spawn(shutdown);
            tokio::pin!(handle);
            loop {
                tx.send(TaskStatus::Running)
                    .expect("Failed to send task status");
                select! {
                    result = &mut handle => {
                        let (again, shutdown) = match result {
                            Ok(Fall::Recoverable { mut task, shutdown, err }) => {
                                let span = tracing::warn_span!("recoverable", task = task_description);
                                let _enter = span.enter();
                                let total = err.chain().len();
                                for (mut index, error) in err.chain().enumerate() {
                                    index+=1;
                                    tracing::warn!(error = format!("Error {index}/{total}"), error);
                                }
                                if tx.send(TaskStatus::Recovering(Arc::new(err))).is_err() {
                                    break;
                                }
                                tracing::warn!("Task Recovering...");
                                task = task.recover().instrument(span.clone()).await.unwrap();
                                tracing::warn!("Task Restarting ↺");
                                (task, shutdown)
                            }

                            Ok(Fall::Unrecoverable { err, exceptional, task }) => {
                                let span = tracing::warn_span!("unrecoverable", task = task_description);
                                let _enter = span.enter();
                                let total = err.chain().len();
                                for (mut index, error) in err.chain().enumerate() {
                                    index+=1;
                                    if exceptional {
                                        tracing::error!(exceptional, error, "Error {index}/{total}");
                                    } else {
                                        tracing::warn!(exceptional, error, "Error {index}/{total}");
                                    }
                                }
                                tracing::warn!("Task Cleaning up..");
                                let _ = task.cleanup().instrument(span.clone()).await;
                                let _ = tx.send(TaskStatus::Stopped{exceptional, err: Arc::new(err)});
                                tracing::warn!("Task Shutting down Ⓧ");
                                break;
                            }

                            Ok(Fall::Shutdown{task}) => {
                                let span = tracing::trace_span!("shutdown", task = task_description);
                                let _entered = span.enter();
                                handle.abort();
                                tracing::trace!("Handle aborted");
                                // then  cleanup
                                let _ = task.cleanup().instrument(span.clone()).await;
                                tracing::trace!("Cleanup ran");
                                // then set status to Stopped
                                let _ = tx.send(TaskStatus::Stopped{exceptional: false, err: Arc::new(eyre::eyre!("Shutdown"))});
                                break;
                            }

                            Err(e) => {
                                let panic_res = e.try_into_panic();

                                if panic_res.is_err() {
                                    tracing::trace!(
                                        task = task_description.as_str(),
                                        "Internal task cancelled",
                                    );
                                    // We don't check the result of the send
                                    // because we're stopping regardless of
                                    // whether it worked
                                    let status = TaskStatus::Stopped{
                                        exceptional: false,
                                        err:Arc::new(eyre::eyre!(panic_res.unwrap_err()))
                                    };
                                    let _ = tx.send(status);
                                    break;
                                }
                                // We don't check the result of the send
                                // because we're stopping regardless of
                                // whether it worked
                                let _ = tx.send(TaskStatus::Panicked);
                                let p = panic_res.unwrap();
                                tracing::error!(task = task_description.as_str(), "Internal task panicked");
                                panic::resume_unwind(p);
                            }
                        };
                        // We use a noisy sleep here to nudge tasks off
                        // eachother if they're crashing around the same time
                        utils::noisy_sleep(again.restart_after_ms()).await;
                        // If we haven't broken from within the match, increment
                        // restarts and push the boulder again.
                        restarts_loop_ref.fetch_add(1, Ordering::Relaxed);
                        *handle = again.spawn(shutdown);
                    },
                }
            }
        });
        Sisyphus {
            restarts,
            status: rx,
            shutdown: shutdown_tx,
            task,
        }
    }
}

#[cfg(test)]
pub(crate) mod test {
    use std::time::Duration;

    use tokio::time::sleep;

    use super::*;

    struct RecoverableTask;
    impl std::fmt::Display for RecoverableTask {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "RecoverableTask")
        }
    }

    impl Boulder for RecoverableTask {
        fn spawn(self, shutdown_tx: ShutdownSignal) -> JoinHandle<Fall<Self>>
        where
            Self: 'static + Send + Sync + Sized,
        {
            tokio::spawn(async move {
                Fall::Recoverable {
                    err: eyre::eyre!("I only took an arrow to the knee"),
                    task: self,
                    shutdown: shutdown_tx,
                }
            })
        }
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_recovery() {
        let handle = RecoverableTask.run_until_panic();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let handle = handle.shutdown();
        let result = handle.await;

        assert!(logs_contain(
            "error=\"Error 1/1\" error=I only took an arrow to the knee"
        ));
        assert!(logs_contain("Task Recovering.."));
        assert!(logs_contain("Task Restarting ↺"));
        assert!(result.is_ok());
    }

    struct UnrecoverableTask;
    impl std::fmt::Display for UnrecoverableTask {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "UnrecoverableTask")
        }
    }

    impl Boulder for UnrecoverableTask {
        fn spawn(self, _shutdown: ShutdownSignal) -> JoinHandle<Fall<Self>> {
            tokio::spawn(async move {
                Fall::Unrecoverable {
                    err: eyre::eyre!("Tis only a scratch"),
                    exceptional: true,
                    task: self,
                }
            })
        }
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_unrecoverable() {
        let handle = UnrecoverableTask.run_until_panic();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = handle.await;
        assert!(logs_contain(
            "Error 1/1 exceptional=true error=Tis only a scratch"
        ));
        assert!(logs_contain("Task Cleaning up.."));
        assert!(logs_contain("Task Shutting down Ⓧ"));
    }

    struct PanicTask;
    impl std::fmt::Display for PanicTask {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "PanicTask")
        }
    }

    impl Boulder for PanicTask {
        fn spawn(self, _shutdown: ShutdownSignal) -> JoinHandle<Fall<Self>>
        where
            Self: 'static + Send + Sync + Sized,
        {
            tokio::spawn(async move { panic!("intentional panic :)") })
        }
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_panic() {
        let handle = PanicTask.run_until_panic();
        let result = handle.await;
        assert!(logs_contain("PanicTask"));
        assert!(logs_contain("Internal task panicked task=\"PanicTask\""));
        assert!(result.is_err() && result.unwrap_err().is_panic());
    }
    struct ShutdownTask {}

    impl std::fmt::Display for ShutdownTask {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "ShutdownTask")
        }
    }

    impl Boulder for ShutdownTask {
        fn spawn(self, shutdown_tx: ShutdownSignal) -> JoinHandle<Fall<Self>>
        where
            Self: 'static + Send + Sync + Sized,
        {
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = sleep(Duration::from_millis(500)) => {
                            shutdown_tx.await.unwrap();
                            sleep(Duration::from_secs(4)).await;
                            return Fall::Unrecoverable {
                                err: eyre::Report::msg("did not shutdown"),
                                exceptional: true,
                                task: self
                            }
                        },
                    }
                }
            })
        }
    }

    #[tokio::test]
    async fn test_shutdown() {
        let handle = ShutdownTask {}.run_until_panic();
        sleep(Duration::from_millis(1000)).await;
        assert_eq!(
            handle.status().to_string(),
            TaskStatus::Stopped {
                exceptional: false,
                err: Arc::new(eyre::Report::msg("Shutdown"))
            }
            .to_string()
        );
    }
}
