//! Web Workers as threads, and a task pool over them.
//!
//! The engine worker's script installs `iw4lThreads` (`web/engine-worker.js`):
//! `spawn(name, stack, entry, module, memory)` starts a worker that
//! instantiates this module over the shared memory and calls the launcher's
//! `iw4l_thread_entry(entry)`, which runs the boxed closure behind `entry`.
//! Without that object (the engine on the page, where a thread may not
//! block) the pool has no threads: scopes run their tasks on the caller and
//! spawned tasks run on the browser's event loop, as Bevy's pool does.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::marker::PhantomData;
use std::mem;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::ThreadId;
use std::time::Duration;

use async_executor::Executor;
use async_task::Task;
use concurrent_queue::ConcurrentQueue;
use futures_lite::future::block_on;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(catch, js_namespace = iw4lThreads, js_name = spawn)]
    fn js_spawn(
        name: &str,
        stack: u32,
        entry: u32,
        module: &JsValue,
        memory: &JsValue,
    ) -> Result<(), JsValue>;
    #[wasm_bindgen(catch, js_namespace = iw4lThreads, js_name = concurrency)]
    fn js_concurrency() -> Result<u32, JsValue>;
}

/// Stack of a spawned thread; wasm-bindgen wants a multiple of 64 KiB. Zone
/// walks and image decodes recurse little, but 4 MiB costs nothing until
/// touched.
const THREAD_STACK: u32 = 4 * 1024 * 1024;

type Entry = Box<dyn FnOnce() + Send + 'static>;

/// The engine thread: the one realm with `iw4lThreads`, so the only thread
/// that can start a worker. Recorded by [`init`].
static ENGINE: OnceLock<ThreadId> = OnceLock::new();
/// 2 unknown, 1 yes, 0 no.
static SUPPORTED: AtomicU8 = AtomicU8::new(2);
static CONCURRENCY: AtomicU32 = AtomicU32::new(1);
/// Spawns asked for from other threads, started by the engine thread on its
/// next frame ([`drain_pending`]).
static PENDING: Mutex<Vec<(String, u32)>> = Mutex::new(Vec::new());

/// Called once on the engine thread (the launcher's worker entry) before
/// anything asks for a thread: records the thread and what the page offers.
pub fn init() {
    let _ = ENGINE.set(std::thread::current().id());
    match js_concurrency() {
        Ok(n) => {
            CONCURRENCY.store(n.max(1), Ordering::Relaxed);
            SUPPORTED.store(1, Ordering::Release);
        }
        Err(_) => SUPPORTED.store(0, Ordering::Release),
    }
}

fn probe() -> bool {
    match SUPPORTED.load(Ordering::Acquire) {
        1 => true,
        0 => false,
        _ => {
            // Not initialised: this is the page (no worker entry ran), where
            // the spawner is absent; remember that.
            init();
            SUPPORTED.load(Ordering::Acquire) == 1
        }
    }
}

/// Whether threads can be spawned: the engine runs in a worker whose script
/// installed `iw4lThreads`. Answered from any thread.
pub fn supported() -> bool {
    probe()
}

/// `navigator.hardwareConcurrency`, or 1 when unknown.
pub fn hardware_concurrency() -> usize {
    probe();
    CONCURRENCY.load(Ordering::Relaxed) as usize
}

fn on_engine_thread() -> bool {
    ENGINE.get() == Some(&std::thread::current().id())
}

/// The engine thread, or any thread before [`init`] named one (the page,
/// single-threaded): where the engine's JavaScript objects live.
pub fn is_device_thread() -> bool {
    ENGINE
        .get()
        .is_none_or(|engine| *engine == std::thread::current().id())
}

/// Starts the workers other threads asked for; the engine thread calls this
/// once a frame (the worker runner).
pub fn drain_pending() {
    let pending = std::mem::take(&mut *PENDING.lock().unwrap_or_else(|e| e.into_inner()));
    for (name, entry) in pending {
        if let Err(error) = js_spawn(
            &name,
            THREAD_STACK,
            entry,
            &wasm_bindgen::module(),
            &wasm_bindgen::memory(),
        ) {
            diag::warn!(Launch, "threads: `{name}` not started: {error:?}");
            // SAFETY: the worker never started, so nobody else owns the entry.
            drop(unsafe { Box::from_raw(entry as *mut Entry) });
        }
    }
}

/// Threads for the load pool: the cores minus two for the engine and the
/// browser, at most six (reads are serialized through one file worker).
/// Worker threads for [`crate::frame_pool`]: a few, since the engine thread
/// works its lanes too and the loaders and audio have threads of their own.
pub fn frame_workers() -> usize {
    if !supported() {
        return 0;
    }
    match FRAME_WORKERS.load(Ordering::Relaxed) {
        usize::MAX => (hardware_concurrency() / 2).clamp(1, 3),
        set => set,
    }
}

static FRAME_WORKERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(usize::MAX);

/// `?framepool=N` on the page: N frame worker threads (0 runs the lanes on
/// the engine thread, as before the pool). Before the pool's first use.
pub fn set_frame_workers(threads: usize) {
    FRAME_WORKERS.store(threads.min(8), Ordering::Relaxed);
}

pub fn load_workers() -> usize {
    if !supported() {
        return 0;
    }
    hardware_concurrency().saturating_sub(2).clamp(1, 6)
}

/// Starts a thread running `f`. The thread's JavaScript scope has the file
/// system and the audio ring, nothing else of the page's.
pub fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> Result<(), String> {
    if !supported() {
        return Err("no thread spawner in this realm".to_owned());
    }
    let entry: Box<Entry> = Box::new(Box::new(f));
    let ptr = Box::into_raw(entry);
    if !on_engine_thread() {
        // Only the engine thread's realm has the spawner: it starts the
        // worker on its next frame.
        PENDING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((name.to_owned(), ptr as u32));
        return Ok(());
    }
    match js_spawn(
        name,
        THREAD_STACK,
        ptr as u32,
        &wasm_bindgen::module(),
        &wasm_bindgen::memory(),
    ) {
        Ok(()) => Ok(()),
        Err(error) => {
            // SAFETY: the worker never started, so nobody else owns `ptr`.
            drop(unsafe { Box::from_raw(ptr) });
            Err(format!("{error:?}"))
        }
    }
}

/// The thread worker's entry (exported by the launcher as
/// `iw4l_thread_entry`): runs the closure `spawn` boxed.
pub fn run_entry(ptr: u32) {
    // SAFETY: `ptr` came from `spawn` and the worker calls this exactly once.
    let entry: Box<Entry> = unsafe { Box::from_raw(ptr as *mut Entry) };
    entry();
}

/// A task pool whose threads are Web Workers sharing this memory.
pub struct WebTaskPool {
    executor: Arc<Executor<'static>>,
    threads: usize,
}

impl WebTaskPool {
    /// `threads` workers named `name`; fewer when spawning fails.
    pub fn new(threads: usize, name: &str) -> Self {
        let executor = Arc::new(Executor::new());
        let mut started = 0;
        for slot in 0..threads {
            let run = executor.clone();
            let label = format!("{name} {slot}");
            match spawn(&label, move || {
                block_on(run.run(futures_lite::future::pending::<()>()));
            }) {
                Ok(()) => started += 1,
                Err(error) => {
                    diag::warn!(Launch, "threads: `{label}` not started: {error}");
                    break;
                }
            }
        }
        diag::info!(
            Launch,
            "threads: pool `{name}` with {started} worker threads ({} cores)",
            hardware_concurrency()
        );
        Self {
            executor,
            threads: started,
        }
    }

    /// Threads working the pool; 1 when the caller is the only one.
    pub fn thread_num(&self) -> usize {
        self.threads.max(1)
    }

    /// Worker threads besides the caller; 0 when the caller runs every task.
    pub fn worker_threads(&self) -> usize {
        self.threads
    }

    /// Runs `future` on the pool, or on the browser's event loop when the
    /// pool has no threads.
    pub fn spawn<T: Send + 'static>(
        &self,
        future: impl Future<Output = T> + Send + 'static,
    ) -> Task<T> {
        if self.threads == 0 {
            let (runnable, task) = async_task::spawn_local(future, |runnable: async_task::Runnable| {
                wasm_bindgen_futures::spawn_local(async move {
                    runnable.run();
                });
            });
            runnable.schedule();
            return task;
        }
        self.executor.spawn(future)
    }

    /// Runs the futures spawned on the scope to completion, this thread
    /// working alongside the pool's, and returns their outputs.
    #[expect(unsafe_code, reason = "lifetimes are transmuted as Bevy's pool does")]
    pub fn scope<'env, F, T>(&self, f: F) -> Vec<T>
    where
        F: for<'scope> FnOnce(&'scope Scope<'scope, 'env, T>),
        T: Send + 'static,
    {
        // SAFETY: every future spawned on the scope completes before this
        // function returns (they are all awaited below), so references the
        // futures hold to `'env` data and to the executor outlive their use.
        let executor: &Executor<'static> = &self.executor;
        let executor: &'env Executor<'env> = unsafe { mem::transmute(executor) };
        let spawned: ConcurrentQueue<Task<T>> = ConcurrentQueue::unbounded();
        let spawned: &'env ConcurrentQueue<Task<T>> = unsafe { mem::transmute(&spawned) };
        let scope = Scope {
            executor,
            spawned,
            scope: PhantomData,
            env: PhantomData,
        };
        let scope: &'env Scope<'_, 'env, T> = unsafe { mem::transmute(&scope) };

        f(scope);

        if spawned.is_empty() {
            return Vec::new();
        }
        let collect = async {
            let mut results = Vec::with_capacity(spawned.len());
            while let Ok(task) = spawned.pop() {
                results.push(task.await);
            }
            results
        };
        if self.threads == 0 {
            // Nobody else runs the executor and this thread may not block:
            // run every task here, after which the outputs are ready.
            while executor.try_tick() {}
            block_on(collect)
        } else {
            block_on(executor.run(collect))
        }
    }
}

impl WebTaskPool {
    /// Bevy's variant with an external executor to tick; here every task
    /// runs on the pool, so it is [`WebTaskPool::scope`].
    pub fn scope_with_executor<'env, F, T>(
        &self,
        _tick_task_pool_executor: bool,
        _external_executor: Option<&bevy::tasks::ThreadExecutor>,
        f: F,
    ) -> Vec<T>
    where
        F: for<'scope> FnOnce(&'scope Scope<'scope, 'env, T>),
        T: Send + 'static,
    {
        self.scope(f)
    }
}

/// A [`WebTaskPool`] scope for futures that borrow from the caller.
pub struct Scope<'scope, 'env: 'scope, T> {
    executor: &'scope Executor<'scope>,
    spawned: &'scope ConcurrentQueue<Task<T>>,
    scope: PhantomData<&'scope mut &'scope ()>,
    env: PhantomData<&'env mut &'env ()>,
}

impl<'scope, 'env, T: Send + 'scope> Scope<'scope, 'env, T> {
    /// Spawns a future whose output is part of the scope's result.
    pub fn spawn<Fut: Future<Output = T> + 'scope + Send>(&self, f: Fut) {
        let task = self.executor.spawn(f);
        // Unbounded and never closed: the push cannot fail.
        let _ = self.spawned.push(task);
    }

    /// Bevy runs these on the scope's own thread; here every thread is alike.
    pub fn spawn_on_scope<Fut: Future<Output = T> + 'scope + Send>(&self, f: Fut) {
        self.spawn(f);
    }
}

// ---- the engine thread's jobs -------------------------------------------------

type EngineJob = Box<dyn FnOnce() + Send + 'static>;

/// Jobs other threads left for the engine thread ([`on_engine`]).
static ENGINE_JOBS: Mutex<Vec<EngineJob>> = Mutex::new(Vec::new());
/// Work the engine thread does every frame for other threads ([`on_engine_frame`]).
static ENGINE_FRAME_HOOKS: Mutex<Vec<(&'static str, fn())>> = Mutex::new(Vec::new());

/// Runs `job` where the page's JavaScript bridges live: at once on the engine
/// thread (or anywhere before [`init`]), otherwise on the engine thread's
/// next frame. For calls into the engine worker's page hooks from a thread
/// whose script does not have them (the game thread, a pool thread).
pub fn on_engine(job: impl FnOnce() + Send + 'static) {
    if is_device_thread() {
        job();
        return;
    }
    ENGINE_JOBS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Box::new(job));
}

/// Has the engine thread call `hook` once every frame from now on (`key`
/// names it, so asking again changes nothing): a bridge another thread polls.
pub fn on_engine_frame(key: &'static str, hook: fn()) {
    let mut hooks = ENGINE_FRAME_HOOKS.lock().unwrap_or_else(|e| e.into_inner());
    if !hooks.iter().any(|(known, _)| *known == key) {
        hooks.push((key, hook));
    }
}

/// The engine thread's turn for other threads, once a frame: the jobs they
/// queued, then the per-frame hooks.
pub fn run_engine_jobs() {
    let jobs = mem::take(&mut *ENGINE_JOBS.lock().unwrap_or_else(|e| e.into_inner()));
    for job in jobs {
        job();
    }
    let hooks: Vec<fn()> = ENGINE_FRAME_HOOKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(_, hook)| *hook)
        .collect();
    for hook in hooks {
        hook();
    }
}

// ---- resident threads ----------------------------------------------------------

/// A thread that stays between steps: [`spawn_resident`].
///
/// A pool thread blocks in its closure for good. A resident thread's
/// closure only stores its step; its script then waits on `go` with
/// `Atomics.waitAsync` and calls `iw4l_thread_step` each time the word
/// changes, returning to its event loop in between. So futures a step
/// spawns there (`spawn_local`: network bridges, asset loads), its timers
/// and its sockets keep running, as they did on the engine thread.
struct ResidentCtl {
    /// Steps asked for (wrapping); the thread's script waits on this word.
    go: AtomicI32,
    /// Steps finished (wrapping).
    done: Mutex<i32>,
    finished: Condvar,
    /// The thread panicked: no step will finish again.
    dead: AtomicBool,
    panic: Mutex<Option<String>>,
}

type Step = Box<dyn FnMut() + Send + 'static>;

thread_local! {
    static RESIDENT: RefCell<Option<(Step, &'static ResidentCtl)>> = const { RefCell::new(None) };
    /// Set on a resident thread: its panic marks it dead.
    static CURRENT_RESIDENT: Cell<Option<&'static ResidentCtl>> = const { Cell::new(None) };
    /// A view of the module's memory for `Atomics.notify`, made once: the
    /// words it reaches were allocated before it.
    static MEMORY_VIEW: RefCell<Option<js_sys::Int32Array>> = const { RefCell::new(None) };
}

/// How [`Resident::wait`] ended.
pub enum Waited {
    Done,
    /// Still working when the time given ran out.
    Busy,
    /// The thread panicked, with what it said.
    Dead(String),
}

/// The handle [`spawn_resident`] returns.
pub struct Resident {
    ctl: &'static ResidentCtl,
}

/// Starts a resident thread whose every step runs `step`. The thread comes
/// up on the engine thread's next frame at the latest; a step asked for
/// before then runs once it is up.
pub fn spawn_resident(name: &str, step: impl FnMut() + Send + 'static) -> Result<Resident, String> {
    install_resident_panic_hook();
    let ctl: &'static ResidentCtl = Box::leak(Box::new(ResidentCtl {
        go: AtomicI32::new(0),
        done: Mutex::new(0),
        finished: Condvar::new(),
        dead: AtomicBool::new(false),
        panic: Mutex::new(None),
    }));
    let step: Step = Box::new(step);
    spawn(name, move || {
        CURRENT_RESIDENT.with(|current| current.set(Some(ctl)));
        RESIDENT.with(|slot| *slot.borrow_mut() = Some((step, ctl)));
    })?;
    Ok(Resident { ctl })
}

/// On a resident thread, the address of the word its script waits on (the
/// launcher's `iw4l_thread_resident`); 0 on any other thread.
pub fn resident_word() -> u32 {
    RESIDENT.with(|slot| {
        slot.borrow()
            .as_ref()
            .map_or(0, |(_, ctl)| ctl.go.as_ptr() as u32)
    })
}

/// One step of this resident thread (the launcher's `iw4l_thread_step`).
pub fn resident_step() {
    let Some((mut step, ctl)) = RESIDENT.with(|slot| slot.borrow_mut().take()) else {
        return;
    };
    step();
    RESIDENT.with(|slot| *slot.borrow_mut() = Some((step, ctl)));
    {
        let mut done = ctl.done.lock().unwrap_or_else(|e| e.into_inner());
        *done = done.wrapping_add(1);
    }
    ctl.finished.notify_all();
}

fn install_resident_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Some(ctl) = CURRENT_RESIDENT.with(Cell::get) {
                if let Ok(mut panic) = ctl.panic.try_lock() {
                    *panic = Some(info.to_string());
                }
                ctl.dead.store(true, Ordering::SeqCst);
                ctl.finished.notify_all();
            }
            previous(info);
        }));
    });
}

impl Resident {
    /// Asks for one step. Not again before [`Self::wait`] said the last one
    /// is done.
    pub fn kick(&self) {
        self.ctl.go.fetch_add(1, Ordering::SeqCst);
        let index = self.ctl.go.as_ptr() as u32 / 4;
        MEMORY_VIEW.with(|view| {
            let mut view = view.borrow_mut();
            let view = view.get_or_insert_with(|| {
                let memory = wasm_bindgen::memory().unchecked_into::<js_sys::WebAssembly::Memory>();
                js_sys::Int32Array::new(&memory.buffer())
            });
            let _ = js_sys::Atomics::notify(view, index);
        });
    }

    /// Whether every step asked for has finished.
    pub fn idle(&self) -> bool {
        *self.ctl.done.lock().unwrap_or_else(|e| e.into_inner())
            == self.ctl.go.load(Ordering::SeqCst)
    }

    /// Waits up to `timeout` for every step asked for to finish.
    pub fn wait(&self, timeout: Duration) -> Waited {
        let target = self.ctl.go.load(Ordering::SeqCst);
        let deadline = web_time::Instant::now() + timeout;
        let mut done = self.ctl.done.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if *done == target {
                return Waited::Done;
            }
            if self.ctl.dead.load(Ordering::SeqCst) {
                let said = self
                    .ctl
                    .panic
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .unwrap_or_else(|| "panicked".to_owned());
                return Waited::Dead(said);
            }
            let left = deadline.saturating_duration_since(web_time::Instant::now());
            if left.is_zero() {
                return Waited::Busy;
            }
            // In slices: a panic notifies without the lock and could slip in
            // between the checks above and the wait.
            done = self
                .ctl
                .finished
                .wait_timeout(done, left.min(Duration::from_millis(20)))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}
