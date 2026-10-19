//! Threads for the browser build.
//!
//! Desktop builds run their load work on a Bevy [`TaskPool`]. The browser
//! build has no `std::thread`; with WebAssembly threads (shared memory and
//! atomics, the `pkg-exp` bundle) a thread is a Web Worker that instantiates
//! the same module over the same memory and runs a boxed closure
//! (`web/engine-thread.js`). [`LoadPool`] is the pool type the loaders take:
//! Bevy's on desktop and on the single-threaded browser build, the worker
//! pool in [`web`] on the threaded one.
//!
//! Why a pool of our own instead of Bevy's: Bevy's pools are single-threaded
//! on every browser build, and its multi-threaded pool spawns through
//! `std::thread`, which wasm32 does not have. Only the engine's explicit load
//! work moves to threads; Bevy's own schedules stay on the engine thread,
//! where the page's JavaScript bridges live.

#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
pub mod web;

/// The pool the loaders run on.
#[cfg(not(all(target_arch = "wasm32", target_feature = "atomics")))]
pub type LoadPool = bevy::tasks::TaskPool;
/// The pool the loaders run on.
#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
pub type LoadPool = web::WebTaskPool;

#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
use std::sync::{Arc, Condvar, Mutex};

/// The pool for a frame's CPU work that a system splits into lanes (skinning
/// the other players, the sun shadow's two cascades, effect collisions):
/// Bevy's compute pool on desktop, where it has threads. On the threaded
/// browser build Bevy's pool is single-threaded, so these lanes get worker
/// threads of their own. Only plain data may cross: no JavaScript or GPU
/// objects, which belong to the engine thread.
pub fn frame_pool() -> &'static LoadPool {
    #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
    {
        static POOL: std::sync::OnceLock<LoadPool> = std::sync::OnceLock::new();
        POOL.get_or_init(|| web::WebTaskPool::new(web::frame_workers(), "iw4l frame"))
    }
    #[cfg(not(all(target_arch = "wasm32", target_feature = "atomics")))]
    {
        bevy::tasks::ComputeTaskPool::get_or_init(bevy::tasks::TaskPool::default)
    }
}

/// Whether this thread may use the engine's JavaScript objects, the browser's
/// GPU device and every GPU handle among them: always, except on a worker
/// thread of the threaded browser build. Work handed to [`frame_pool`] that
/// would need them reports so, and its caller redoes it on this thread.
pub fn on_device_thread() -> bool {
    #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
    {
        web::is_device_thread()
    }
    #[cfg(not(all(target_arch = "wasm32", target_feature = "atomics")))]
    {
        true
    }
}

/// The machine's logical processors: the system's count on desktop,
/// `navigator.hardwareConcurrency` in the browser (on the threaded build the
/// page's `?cores=` stand-in when it gives one).
pub fn logical_processors() -> usize {
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::thread::available_parallelism().map_or(4, |n| n.get())
    }
    #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
    {
        web::hardware_concurrency()
    }
    #[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
    {
        let navigator = js_sys::Reflect::get(&js_sys::global(), &"navigator".into()).ok();
        navigator
            .and_then(|navigator| js_sys::Reflect::get(&navigator, &"hardwareConcurrency".into()).ok())
            .and_then(|count| count.as_f64())
            .map_or(4, |count| (count as usize).max(1))
    }
}

/// How many lanes a frame's split work should use: the threads that can run
/// them at once, the calling thread included.
pub fn frame_lanes() -> usize {
    #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
    {
        frame_pool().worker_threads() + 1
    }
    #[cfg(not(all(target_arch = "wasm32", target_feature = "atomics")))]
    {
        static LANES: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        *LANES.get_or_init(|| {
            std::thread::available_parallelism()
                .map(|n| n.get().saturating_sub(1).max(1))
                .unwrap_or(1)
        })
    }
}

/// A named thread: `std`'s on desktop, a Web Worker sharing the memory on
/// the threaded browser build. [`spawn_named`] fails on browser builds
/// without threads, and the caller does the work inline instead.
pub struct Handle(HandleInner);

enum HandleInner {
    #[cfg(not(target_arch = "wasm32"))]
    Std(std::thread::JoinHandle<()>),
    #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
    Web(Arc<WebThread>),
    /// A browser build without threads never makes one; the type still
    /// needs a shape for the code that would.
    #[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
    Absent,
}

#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
struct WebThread {
    /// Registered by the thread itself once it runs; `unpark` before that
    /// is a no-op, and the thread's first pass runs regardless.
    thread: Mutex<Option<std::thread::Thread>>,
    done: (Mutex<bool>, Condvar),
}

impl Handle {
    /// Wakes the thread from `std::thread::park`.
    pub fn unpark(&self) {
        match &self.0 {
            #[cfg(not(target_arch = "wasm32"))]
            HandleInner::Std(handle) => handle.thread().unpark(),
            #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
            HandleInner::Web(thread) => {
                if let Some(thread) = thread
                    .thread
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                {
                    thread.unpark();
                }
            }
            #[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
            HandleInner::Absent => {}
        }
    }

    /// Waits for the thread to finish; `Err` when it panicked.
    pub fn join(self) -> Result<(), String> {
        match self.0 {
            #[cfg(not(target_arch = "wasm32"))]
            HandleInner::Std(handle) => handle.join().map_err(|_| "thread panicked".to_owned()),
            #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
            HandleInner::Web(thread) => {
                let (done, finished) = &thread.done;
                let mut done = done.lock().unwrap_or_else(|e| e.into_inner());
                while !*done {
                    done = finished.wait(done).unwrap_or_else(|e| e.into_inner());
                }
                Ok(())
            }
            #[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
            HandleInner::Absent => Ok(()),
        }
    }
}

/// Starts a thread named `name` running `f`.
pub fn spawn_named(name: &str, f: impl FnOnce() + Send + 'static) -> Result<Handle, String> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(f)
            .map(|handle| Handle(HandleInner::Std(handle)))
            .map_err(|error| error.to_string())
    }
    #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
    {
        let state = Arc::new(WebThread {
            thread: Mutex::new(None),
            done: (Mutex::new(false), Condvar::new()),
        });
        let inside = state.clone();
        web::spawn(name, move || {
            *inside.thread.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(std::thread::current());
            f();
            *inside.done.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
            inside.done.1.notify_all();
        })?;
        Ok(Handle(HandleInner::Web(state)))
    }
    #[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
    {
        let _ = (name, f);
        Err("this browser build has no threads".to_owned())
    }
}

/// Runs `job` on the thread that owns the page's JavaScript bridges (the
/// engine worker's page hooks): at once natively or on that thread, else on
/// its next frame. See [`web::on_engine`].
pub fn on_engine(job: impl FnOnce() + Send + 'static) {
    #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
    web::on_engine(job);
    #[cfg(not(all(target_arch = "wasm32", target_feature = "atomics")))]
    job();
}

/// Whether this build runs its load work on real threads here.
pub fn threaded() -> bool {
    #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
    {
        web::supported()
    }
    #[cfg(not(all(target_arch = "wasm32", target_feature = "atomics")))]
    {
        !cfg!(target_arch = "wasm32")
    }
}
