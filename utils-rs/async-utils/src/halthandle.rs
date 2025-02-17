// Copyright (C) 2020  Braiins Systems s.r.o.
//
// This file is part of Braiins Open-Source Initiative (BOSI).
//
// BOSI is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// Please, keep in mind that we may also license BOSI or any part thereof
// under a proprietary license. For more information on the terms and conditions
// of such proprietary license or if you have any other questions, please
// contact us at opensource@braiins.com.

use std::error::Error as StdError;
use std::fmt;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::prelude::*;
use tokio::sync::{mpsc, watch, Notify};
use tokio::task::{JoinError, JoinHandle};
use tokio::time;
use tokio_stream::wrappers::UnboundedReceiverStream;

#[cfg(target_family = "unix")]
async fn interrupt_signal<FT>(ft: FT)
where
    FT: Future + Send + 'static,
{
    use tokio::signal::unix;
    use tokio_stream::wrappers::SignalStream;

    let sigterm = SignalStream::new(
        unix::signal(unix::SignalKind::terminate()).expect("BUG: Error listening for SIGTERM"),
    );
    let sigint = SignalStream::new(
        unix::signal(unix::SignalKind::interrupt()).expect("BUG: Error listening for SIGINT"),
    );

    future::select(sigterm.into_future(), sigint.into_future()).await;
    ft.await;
}

#[cfg(target_family = "windows")]
async fn interrupt_signal<FT>(ft: FT)
where
    FT: Future + Send + 'static,
{
    use tokio::signal::windows;
    use tokio_stream::wrappers::{CtrlBreakStream, CtrlCStream};

    let sigterm = CtrlCStream::new(windows::ctrl_c().expect("BUG: Error listening for SIGTERM"));
    let sigint =
        CtrlBreakStream::new(windows::ctrl_break().expect("BUG: Error listening for SIGINT"));

    future::select(sigterm.into_future(), sigint.into_future()).await;
    ft.await;
}

#[cfg(all(not(target_family = "unix"), not(target_family = "windows")))]
compile_error!("Unsupported OS family");

pub trait Spawnable {
    fn run(self, tripwire: Tripwire) -> JoinHandle<()>;
}

/// Internal, used to signal termination via `trigger`
/// and notify `Tasks` when that happens.
#[derive(Debug)]
struct Halt {
    trigger: Trigger,
    notify_join: Arc<Notify>,
}

/// Internal, used in the `Tasks` channel,
/// contains either a join handle of a task
/// that was spawned or a ready notification which
/// indicates to the `join()` function that all necessary tasks
/// were spawned.
///
/// `spawn()` uses this to send a spawned task's handle,
/// `ready()` to send a Ready notification.
#[derive(Debug)]
enum TaskMsg {
    Task(JoinHandle<()>),
    Ready,
}

/// Internal, used in `HaltHandle::join()`
/// to wait on signal from `halt()`
/// and then collect halting tasks' join handles.
#[derive(Debug)]
struct Tasks {
    tasks_rx: UnboundedReceiverStream<TaskMsg>,
    notify_join: Arc<Notify>,
}

/// Error type returned by `HaltHandle::join()`.
#[derive(Debug)]
pub enum HaltError {
    /// Tasks didn't finish inside the timeout passed to `join()`.
    Timeout,
    /// One of the tasks panicked.
    Join(JoinError),
}

impl HaltError {
    fn map<'a, T, F: FnOnce(&'a JoinError) -> Option<T>>(&'a self, f: F) -> Option<T> {
        match self {
            HaltError::Timeout => None,
            HaltError::Join(err) => f(err),
        }
    }
}

impl fmt::Display for HaltError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HaltError::Timeout => write!(fmt, "Timeout"),
            HaltError::Join(err) => write!(fmt, "Join error: {}", err),
        }
    }
}

impl StdError for HaltError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.map(JoinError::source)
    }

    #[allow(deprecated)]
    fn cause(&self) -> Option<&dyn StdError> {
        self.map(JoinError::cause)
    }
}

/// A synchronization end that can be used to
/// cancel tasks which use the associated `Tripwire` instance.
///
/// NB. This is really just a thin wrapper around `watch::Sender`.
#[derive(Debug)]
pub struct Trigger(watch::Sender<bool>);

impl Trigger {
    pub fn cancel(self) {
        let _ = self.0.send(true);
    }
}

type WaitForHaltFuture =
    Pin<Box<dyn Future<Output = Result<(), watch::error::RecvError>> + Send + Sync>>;

/// A synchronization end that tasks can use to wait on
/// using eg. `take_until()` or `select!()` or similar
/// to await cancellation.
///
/// NB. This is really just a thin wrapper around `watch::Receiver`.
pub struct Tripwire {
    receiver: Option<watch::Receiver<bool>>,
    wait_for_halt_future: Option<WaitForHaltFuture>,
}

impl Tripwire {
    pub fn new() -> (Trigger, Self) {
        let (sender, receiver) = watch::channel(false);
        (
            Trigger(sender),
            Tripwire {
                receiver: Some(receiver),
                wait_for_halt_future: None,
            },
        )
    }

    async fn wait_for_halt(
        mut receiver: watch::Receiver<bool>,
    ) -> Result<(), watch::error::RecvError> {
        loop {
            receiver.changed().await?;
            if *receiver.borrow() {
                break;
            }
        }
        Ok(())
    }
}

impl Clone for Tripwire {
    fn clone(&self) -> Self {
        Self {
            receiver: self.receiver.clone(),
            wait_for_halt_future: None,
        }
    }
}

impl Future for Tripwire {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let receiver = match &self.receiver {
            Some(receiver) => receiver.clone(),
            None => return Poll::Ready(()),
        };
        let wait_for_halt_future = self
            .wait_for_halt_future
            .get_or_insert_with(|| Box::pin(Self::wait_for_halt(receiver)));
        match wait_for_halt_future.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(_) => {
                self.receiver.take();
                self.wait_for_halt_future.take();
                Poll::Ready(())
            }
        }
    }
}

/// A handle with which tasks can be spawned and then halted.
///
/// # Usage
/// 1. Create a `HaltHandle` with `HaltHandle::new()` or `HaltHandle::arc()`
/// (use the latter if you want to share it between tasks or use `halt_on_signal()`).
/// 2. Spawn any number of tasks using the `spawn()` method.
/// 3. When all relevant `spawn()` calls were made, use the `ready()` method
///    to tell the `HaltHandle` that all tasks were spawned.
/// 4. Use `halt()` to tell the spawned tasks that they should stop.
///    You can also use `halt_on_signal()`, which will setup a
///    handler that calls `halt()` on `SIGTERM` & `SIGINT`.
/// 5. Use `join()` to wait on the tasks to stop (a timeout may be used).
///
/// Note that `halt()` or `halt_on_signal()` doesn't necessarily need to be called
/// after `ready()`. These can be called pretty much anytime and it won't cause
/// a race condition as long as `ready()` is called in the right moment.
pub struct HaltHandle {
    /// Tripwire that is cloned into
    /// 'child' tasks when they are started with this handle.
    tripwire: Tripwire,
    /// Used to trigger the tripwire and then notifies `tasks`.
    halt: Mutex<Option<Halt>>,
    /// Spawned task handles as well as a ready notification are sent here, see `TaskMsg`
    tasks_tx: mpsc::UnboundedSender<TaskMsg>,
    /// Used to receive notification from `halt` and the task handles.
    tasks: Mutex<Option<Tasks>>,
    /// A flag whether we've already spawned a signal task;
    /// this can only be done once.
    signal_task_spawned: AtomicBool,
}

impl Default for HaltHandle {
    fn default() -> Self {
        let (trigger, tripwire) = Tripwire::new();
        let notify_join = Arc::new(Notify::new());
        let (tasks_tx, tasks_rx) = mpsc::unbounded_channel();

        Self {
            tripwire,
            halt: Mutex::new(Some(Halt {
                trigger,
                notify_join: notify_join.clone(),
            })),
            tasks_tx,
            tasks: Mutex::new(Some(Tasks {
                tasks_rx: UnboundedReceiverStream::new(tasks_rx),
                notify_join,
            })),
            signal_task_spawned: AtomicBool::new(false),
        }
    }
}

impl HaltHandle {
    /// Create a new `HaltHandle`
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a `HaltHandle` and wrap it in `Arc` for sharing between tasks
    pub fn arc() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Spawn a new task. `f` is a function that takes
    /// a `Tripwire` and returns a `Future` to be spawned.
    /// `Tripwire` can be passed to `StreamExt::take_until`
    /// to make a stream stop generating items when
    /// `halt()` is called on the `HaltHandle`.
    pub fn spawn<FT, FN>(&self, f: FN)
    where
        FT: Future<Output = ()> + Send + 'static,
        FN: FnOnce(Tripwire) -> FT,
    {
        let ft = f(self.tripwire());
        self.add_task(tokio::spawn(ft));
    }

    pub fn spawn_object<T: Spawnable>(&self, obj: T) {
        self.add_task(obj.run(self.tripwire()));
    }

    pub fn tripwire(&self) -> Tripwire {
        self.tripwire.clone()
    }

    pub fn add_task(&self, task: JoinHandle<()>) {
        // Add the task join handle to tasks_tx (used by join()).
        // Errors are ignored here - send() on an unbounded channel
        // only fails if the receiver is dropped, and in that case
        // we don't care that the send() failed...
        let _ = self.tasks_tx.send(TaskMsg::Task(task));
    }

    /// Tells the handle that all tasks were spawned
    pub fn ready(&self) {
        // Send a Ready message. join() uses this to tell
        // that enough join handles were collected.
        // Error is ignored here for the same reason as in spawn().
        let _ = self.tasks_tx.send(TaskMsg::Ready);
    }

    /// Tell the handle to halt all the associated tasks.
    pub fn halt(&self) {
        if let Some(halt) = self
            .halt
            .lock()
            .expect("BUG: HaltHandle: Poisoned mutex")
            .take()
        {
            halt.trigger.cancel();
            halt.notify_join.notify_one();
        }
    }

    pub fn halt_on_signal(self: &Arc<Self>) {
        Self::handle_signal(self.clone(), |this| async move { this.halt() });
    }

    /// Tell the handle to catch `SIGTERM` & `SIGINT` and run
    /// the future generated by `f` when the signal is received.
    pub fn handle_signal<FT, FN>(self: Arc<Self>, f: FN)
    where
        FT: Future + Send + 'static,
        FN: FnOnce(Arc<Self>) -> FT,
    {
        if self
            .signal_task_spawned
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            let ft = f(self);
            tokio::spawn(interrupt_signal(ft));
        }
    }

    /// Wait for all associated tasks to finish.
    /// Call this function once `ready()` was called on the handle.
    /// It will collect task results once they are stopped with `halt()` or once
    /// they finish by themselves.
    ///
    /// An optional `timeout` may be provided, this is the maximum time
    /// to wait **after** `halt()` has been called.
    ///
    /// Returns `Ok(())` when tasks are collected succesfully, or a `HaltError::Timeout`
    /// if tasks tasks didn't stop in time, or a `HaltError::Join` when a task panics.
    /// If multiple tasks panic, the first join error encountered is returned.
    ///
    /// # Panics
    /// `join()` panics if you call it multiple times. It must only be called once.
    pub async fn join(&self, timeout: Option<Duration>) -> Result<(), HaltError> {
        let tasks = self
            .tasks
            .lock()
            .expect("BUG: HaltHandle: Poisoned mutex")
            .take()
            .expect("BUG: HaltHandle: join() called multiple times");

        let Tasks {
            tasks_rx,
            notify_join,
        } = tasks;

        // Map the incomming handles stream (up to the Ready mesage) into a future
        // that awaits them and fails fast if there's a join error.
        let handles = tasks_rx
            .take_while(|task_msg| future::ready(!matches!(task_msg, TaskMsg::Ready)))
            .map(|msg| match msg {
                TaskMsg::Task(handle) => handle,
                TaskMsg::Ready => unreachable!("BUG: Unexpected Ready message"),
            })
            .fold(Ok(()), |res, handle| async {
                if res.is_ok() {
                    handle.await.map_err(HaltError::Join)
                } else {
                    res
                }
            });

        // Waits for notify_join and then starts to apply the timeout, if any
        let notify = async move {
            let _ = notify_join.notified().await;
            // At this point halt() is confirmed to have been called...
            if let Some(timeout) = timeout {
                time::sleep(timeout).await;
                Err(HaltError::Timeout)
            } else {
                future::pending().await
            }
        };

        tokio::select! {
            res = handles => res,
            timeout = notify => timeout,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    use tokio::time;

    /// Wait indefinitely on a stream with a `Tripwire` for cancellation.
    async fn forever_stream(tripwire: Tripwire) {
        let mut stream = tokio_stream::pending::<()>().take_until(tripwire);

        // The pending stream never actually yields a value,
        // ie. next() resolves to None only in the canelled case,
        // otherwise it doesn't return at all.
        stream.next().await;
    }

    // Basic functional test
    #[tokio::test]
    async fn halthandle_basic() {
        let handle = HaltHandle::new();

        // Spawn a couple of tasks on the handle
        for _ in 0..10 {
            handle.spawn(|tripwire| forever_stream(tripwire));
        }

        // Signal ready, halt, and join tasks
        handle.ready();
        handle.halt();
        handle.join(None).await.expect("BUG: join() failed");
    }

    // Test that Tripwire won't abort a task right away
    // without halt() being called (this was a bug).
    #[tokio::test]
    async fn halthandle_tripwire_bug() {
        let handle = HaltHandle::new();

        let task_done = Arc::new(AtomicBool::new(false));

        // Spawn a couple of tasks on the handle
        let task_done2 = task_done.clone();
        handle.spawn(move |tripwire| async move {
            forever_stream(tripwire).await;
            task_done2.store(true, Ordering::SeqCst);
        });

        // Signal ready
        handle.ready();

        // Delay a bit so that the task has time to exit if it is to exit
        time::sleep(Duration::from_millis(500)).await;

        // Verify task didn't exit
        assert_eq!(task_done.load(Ordering::SeqCst), false);
    }

    // The same as basic test but with halting happening from within a task.
    // In this case the `HaltHandle` is shared in an `Arc`.
    #[tokio::test]
    async fn halthandle_shared() {
        let handle = HaltHandle::arc();

        // Spawn a couple of tasks on the handle
        for _ in 0..10 {
            handle.spawn(|tripwire| forever_stream(tripwire));
        }

        // Spawn a task that will halt()
        let handle2 = handle.clone();
        handle.spawn(|_| async move {
            handle2.halt();
        });

        // Join tasks
        handle.ready();
        handle.join(None).await.expect("BUG: join() failed");
    }

    // Test that spawn() / halt() / join() is not racy when ready()
    // is used appropriately.
    #[tokio::test(flavor = "multi_thread")]
    async fn halthandle_race() {
        const NUM_TASKS: usize = 10;

        let handle = HaltHandle::arc();
        let num_cancelled = Arc::new(AtomicUsize::new(0));

        // Signal halt right away, this should be fine
        handle.halt();

        // Spawn tasks in another task to allow a race
        {
            let handle = handle.clone();
            let num_cancelled = num_cancelled.clone();

            tokio::spawn(async move {
                // Delay a bit so that join() happens sooner than spawns
                time::sleep(Duration::from_millis(100)).await;

                // Spawn a couple of tasks on the handle
                for _ in 0..NUM_TASKS {
                    let num_cancelled = num_cancelled.clone();
                    handle.spawn(|tripwire| async move {
                        forever_stream(tripwire).await;
                        num_cancelled.fetch_add(1, Ordering::SeqCst);
                    });
                }

                // Finally, signal that tasks are ready
                handle.ready();
            });
        }

        // Join tasks
        handle.join(None).await.expect("BUG: join() failed");

        let num_cancelled = num_cancelled.load(Ordering::SeqCst);
        assert_eq!(num_cancelled, NUM_TASKS);
    }

    // Test that if cleanup after halt takes too long, handler will return the right error
    #[tokio::test]
    async fn halthandle_timeout() {
        let handle = HaltHandle::new();

        handle.spawn(|tripwire| {
            async {
                forever_stream(tripwire).await;

                // Delay cleanup on purpose here
                time::sleep(Duration::from_secs(9001)).await;
            }
        });

        handle.ready();
        handle.halt();
        let res = handle.join(Some(Duration::from_millis(100))).await;

        // Verify we've got a timeout
        match &res {
            Err(HaltError::Timeout) => (),
            _ => panic!(
                "BUG: join result was supposed to be HaltError::Timeout but was instead: {:?}",
                res
            ),
        }
    }

    // Test that join() resolves when tasks finish by themselves,
    // ie. without halt() being called
    #[tokio::test]
    async fn halthandle_no_halt() {
        let handle = HaltHandle::new();

        // Spawn a few tasks which are ready right away
        for _ in 0..10 {
            handle.spawn(|_| future::ready(()));
        }

        // Signal ready and join tasks
        handle.ready();
        handle.join(None).await.expect("BUG: join() failed");
    }

    // Verify panicking works
    #[tokio::test]
    async fn halthandle_panic() {
        let handle = HaltHandle::new();

        // Spawn a panicking task
        handle.spawn(|_| async {
            panic!("Things aren't going well");
        });

        handle.ready();
        handle.halt();
        let res = handle.join(Some(Duration::from_millis(100))).await;

        // Verify we've got a join error
        match &res {
            Err(HaltError::Join(_)) => (),
            _ => panic!(
                "BUG: join result was supposed to be HaltError::Join but was instead: {:?}",
                res
            ),
        }
    }
}
