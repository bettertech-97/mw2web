//! The async runtime the master bridge runs on. On desktop that is tokio on the
//! bridge thread. The browser has one thread and no tokio timer or task
//! driver, so the same names are backed by `setTimeout` and the page's own
//! microtask queue (`wasm_bindgen_futures::spawn_local`).

#[cfg(not(target_arch = "wasm32"))]
pub use tokio::task::{JoinError, JoinSet};
#[cfg(not(target_arch = "wasm32"))]
pub use tokio::time::{Instant, MissedTickBehavior, interval, sleep, sleep_until, timeout};

#[cfg(not(target_arch = "wasm32"))]
pub fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(future)
}

#[cfg(target_arch = "wasm32")]
pub use web::*;

#[cfg(target_arch = "wasm32")]
mod web {
    use std::cell::RefCell;
    use std::fmt;
    use std::future::Future;
    use std::task::{Poll, Waker};
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;
    use wasm_bindgen::prelude::*;
    pub use web_time::Instant;

    #[wasm_bindgen]
    extern "C" {
        #[wasm_bindgen(js_name = setTimeout)]
        fn set_timeout(callback: &js_sys::Function, ms: i32) -> JsValue;
    }

    /// Longest single timer; a sleep is a chain of these.
    const STEP: Duration = Duration::from_millis(100);

    thread_local! {
        /// Timers the engine's frame loop also fires; see [`pump`].
        static DUE: RefCell<Vec<(Instant, Waker)>> = const { RefCell::new(Vec::new()) };
    }

    /// Wakes every timer that is due. The engine calls this once a frame:
    /// a background tab throttles chained `setTimeout`s to one a second and
    /// later one a minute, which would starve the bridge while the engine,
    /// whose frames are not throttled the same way, keeps running.
    pub fn pump() {
        let now = Instant::now();
        let due: Vec<Waker> = DUE.with(|timers| {
            let mut timers = timers.borrow_mut();
            let mut due = Vec::new();
            timers.retain(|(at, waker)| {
                if *at <= now {
                    due.push(waker.clone());
                    false
                } else {
                    true
                }
            });
            due
        });
        for waker in due {
            waker.wake();
        }
    }

    async fn timer(duration: Duration) {
        let deadline = Instant::now() + duration;
        let ms = duration.as_millis().min(i32::MAX as u128) as i32;
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            set_timeout(&resolve, ms);
        });
        let frame = std::future::poll_fn(|cx| {
            if Instant::now() >= deadline {
                return Poll::Ready(());
            }
            DUE.with(|timers| timers.borrow_mut().push((deadline, cx.waker().clone())));
            Poll::Pending
        });
        tokio::select! {
            _ = wasm_bindgen_futures::JsFuture::from(promise) => {}
            () = frame => {}
        }
    }

    /// Sleeps for `duration` of time the page was responsive. The engine
    /// shares this thread and can hold it for many seconds while it loads a
    /// map; that stall counts as one step, not against a deadline. Otherwise
    /// an 8 s I/O deadline would expire before the socket's own reply, which
    /// arrived during the stall, could even be read.
    pub async fn sleep(duration: Duration) {
        let mut left = duration;
        while !left.is_zero() {
            let step = left.min(STEP);
            let started = Instant::now();
            timer(step).await;
            left = left.saturating_sub(started.elapsed().clamp(step, step * 2));
        }
    }

    pub async fn sleep_until(deadline: Instant) {
        sleep(deadline.saturating_duration_since(Instant::now())).await;
    }

    #[derive(Debug)]
    pub struct Elapsed;

    impl fmt::Display for Elapsed {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("deadline has elapsed")
        }
    }

    impl std::error::Error for Elapsed {}

    pub async fn timeout<F: Future>(duration: Duration, future: F) -> Result<F::Output, Elapsed> {
        tokio::select! {
            output = future => Ok(output),
            () = sleep(duration) => Err(Elapsed),
        }
    }

    #[derive(Clone, Copy, Debug)]
    pub enum MissedTickBehavior {
        Delay,
    }

    /// tokio's `Interval` with `Delay`: the first tick is immediate, the rest
    /// one period after the previous tick completed.
    pub struct Interval {
        period: Duration,
        next: Instant,
    }

    pub fn interval(period: Duration) -> Interval {
        Interval {
            period,
            next: Instant::now(),
        }
    }

    impl Interval {
        pub fn set_missed_tick_behavior(&mut self, _behavior: MissedTickBehavior) {}

        pub async fn tick(&mut self) -> Instant {
            sleep_until(self.next).await;
            let now = Instant::now();
            self.next = now + self.period;
            now
        }
    }

    #[derive(Debug)]
    pub struct JoinError;

    impl fmt::Display for JoinError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("task cancelled")
        }
    }

    /// Tasks on the page's event loop whose outputs come back in completion
    /// order. Dropping the set cancels what is still running.
    pub struct JoinSet<T> {
        done_tx: tokio::sync::mpsc::UnboundedSender<T>,
        done_rx: tokio::sync::mpsc::UnboundedReceiver<T>,
        running: usize,
        cancel: CancellationToken,
    }

    impl<T: 'static> JoinSet<T> {
        pub fn new() -> Self {
            let (done_tx, done_rx) = tokio::sync::mpsc::unbounded_channel();
            Self {
                done_tx,
                done_rx,
                running: 0,
                cancel: CancellationToken::new(),
            }
        }

        pub fn spawn(&mut self, future: impl Future<Output = T> + 'static) {
            self.running += 1;
            let done = self.done_tx.clone();
            let cancel = self.cancel.clone();
            wasm_bindgen_futures::spawn_local(async move {
                tokio::select! {
                    () = cancel.cancelled() => {}
                    output = future => {
                        let _ = done.send(output);
                    }
                }
            });
        }

        /// `None` once nothing is running, like tokio's.
        pub async fn join_next(&mut self) -> Option<Result<T, JoinError>> {
            if self.running == 0 {
                return None;
            }
            let output = self.done_rx.recv().await?;
            self.running -= 1;
            Some(Ok(output))
        }
    }

    impl<T> Drop for JoinSet<T> {
        fn drop(&mut self) {
            self.cancel.cancel();
        }
    }

    pub struct AbortHandle(CancellationToken);

    impl AbortHandle {
        pub fn abort(&self) {
            self.0.cancel();
        }
    }

    pub fn spawn(future: impl Future<Output = ()> + 'static) -> AbortHandle {
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        wasm_bindgen_futures::spawn_local(async move {
            tokio::select! {
                () = token.cancelled() => {}
                () = future => {}
            }
        });
        AbortHandle(cancel)
    }
}
