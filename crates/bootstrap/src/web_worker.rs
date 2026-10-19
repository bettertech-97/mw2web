//! The engine inside a Web Worker.
//!
//! The page keeps the DOM: it owns the canvas element, the input events,
//! pointer lock, the audio context and the decoder. It transfers the canvas
//! to this worker as an `OffscreenCanvas` and forwards input as messages
//! (`web/engine-worker.js` installs the receiving functions). Here that
//! canvas becomes the primary window's surface, the forwarded input becomes
//! Bevy's input messages, and `requestAnimationFrame` on the worker's own
//! scope drives the frame. No winit: there is no DOM to listen to.
//!
//! Why: the page thread stays free, so a long load never freezes the tab,
//! the audio ring is fed regardless of what the engine is doing, and the
//! engine may block (`Atomics.wait`) on a file read or a lock, which is
//! what makes real threads inside the engine possible later.

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::Mutex;

use bevy::app::PluginsState;
use bevy::input::ButtonState;
use bevy::input::keyboard::{Key, KeyCode, KeyboardInput, NativeKey, NativeKeyCode};
use bevy::input::mouse::{MouseButton, MouseButtonInput, MouseMotion, MouseScrollUnit, MouseWheel};
use bevy::input::touch::TouchPhase;
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy::window::{
    CursorEntered, CursorGrabMode, CursorMoved, CursorOptions, PrimaryWindow, RawHandleWrapper,
    WindowFocused, WindowResized, WindowWrapper,
};
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, WebDisplayHandle, WebOffscreenCanvasWindowHandle, WindowHandle,
};
use wasm_bindgen::prelude::*;

/// What the worker entry hands over before the app is built.
struct Host {
    canvas: JsValue,
    width: u32,
    height: u32,
    scale: f64,
}

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
    /// Stays set after the plugin took the canvas out of [`HOST`].
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    /// Free-running frames: the channel whose message starts the next one.
    static NEXT_FRAME: RefCell<Option<web_sys::MessageChannel>> = const { RefCell::new(None) };
    static FRAME_CLOCK: RefCell<FrameClock> = RefCell::new(FrameClock::default());
    static PAGE_STATS: RefCell<PageFrameStats> = RefCell::new(PageFrameStats::default());
    static PACER: RefCell<FramePacer> = RefCell::new(FramePacer::default());
}

static EVEN_PACING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// `?frames=every`: a frame on every display refresh, as before even pacing.
pub fn set_even_pacing(on: bool) {
    EVEN_PACING.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Even frame pacing. Every display refresh offers a frame; an engine that
/// cannot finish one per refresh shows an uneven mix of one- and
/// two-refresh frames (75-85 fps on a 120 Hz display), which reads as worse
/// than a steady 60. So frames run every `divisor` refreshes, one more when
/// over 8% of frames miss their slot, never so many that fewer than 50
/// frames a second remain (a 60 Hz display is not halved). Faster is a
/// trial: if the misses come back, it is undone and tried again later,
/// waiting twice as long each time.
struct FramePacer {
    /// The animation frame's timestamp: the display refresh it belongs to,
    /// so gaps between them are whole refresh periods.
    last_stamp: Option<f64>,
    last_run_stamp: Option<f64>,
    window_start: Option<web_time::Instant>,
    intervals: Vec<f32>,
    costs: Vec<f32>,
    refresh_ms: f32,
    divisor: u32,
    calm: u32,
    up_wait: u32,
    trial: u32,
    /// Gaps between the frames that ran, in refreshes (1, 2, 3 or more),
    /// until the frame clock takes them: how evenly frames reached the screen.
    gaps: [u32; 3],
    /// Frames run this window, and those that reached the screen later than
    /// `divisor` refreshes after the one before (missed their slot).
    window_frames: u32,
    window_misses: u32,
}

impl Default for FramePacer {
    fn default() -> Self {
        Self {
            last_stamp: None,
            last_run_stamp: None,
            window_start: None,
            intervals: Vec::new(),
            costs: Vec::new(),
            refresh_ms: 1000.0 / 60.0,
            divisor: 1,
            calm: 0,
            up_wait: 3,
            trial: 0,
            gaps: [0; 3],
            window_frames: 0,
            window_misses: 0,
        }
    }
}

impl FramePacer {
    /// At a display refresh (`stamp`, the animation frame's timestamp, when
    /// the frame came from one): whether to run a frame now.
    fn tick(&mut self, stamp: Option<f64>) -> bool {
        let Some(stamp) = stamp else {
            return true;
        };
        if let Some(last) = self.last_stamp {
            let ms = (stamp - last) as f32;
            if (2.0..100.0).contains(&ms) {
                self.intervals.push(ms);
            }
        }
        self.last_stamp = Some(stamp);
        if self.divisor <= 1
            || !EVEN_PACING.load(std::sync::atomic::Ordering::Relaxed)
            || FREE_RUNNING.load(std::sync::atomic::Ordering::Relaxed)
        {
            return true;
        }
        let Some(last) = self.last_run_stamp else {
            return true;
        };
        // Half a refresh of slack against timestamp jitter.
        (stamp - last) as f32 >= (self.divisor as f32 - 0.5) * self.refresh_ms
    }

    /// After a frame of refresh `stamp` that started at `started` and cost
    /// `cost_ms`.
    fn ran(&mut self, stamp: Option<f64>, started: web_time::Instant, cost_ms: f32) {
        if let (Some(stamp), Some(last)) = (stamp, self.last_run_stamp) {
            let gap = ((stamp - last) as f32 / self.refresh_ms).round() as usize;
            self.gaps[gap.clamp(1, 3) - 1] += 1;
            self.window_frames += 1;
            if gap > self.divisor as usize {
                self.window_misses += 1;
            }
        }
        self.last_run_stamp = stamp;
        if FREE_RUNNING.load(std::sync::atomic::Ordering::Relaxed) || stamp.is_none() {
            return;
        }
        self.costs.push(cost_ms);
        let window_start = *self.window_start.get_or_insert(started);
        if (started - window_start).as_secs_f32() < 1.0 || self.costs.len() < 10 {
            return;
        }
        self.window_start = Some(started);
        if self.intervals.len() >= 10 {
            // The display's period: the shortest gaps between refresh
            // timestamps (longer ones are frames that ran over, or skipped).
            self.intervals.sort_unstable_by(f32::total_cmp);
            self.refresh_ms = self.intervals[self.intervals.len() / 20].clamp(4.0, 34.0);
        }
        self.costs.sort_unstable_by(f32::total_cmp);
        let p50 = self.costs[self.costs.len() / 2];
        let p90 = self.costs[self.costs.len() * 9 / 10];
        let missed = self.window_misses as f32 / self.window_frames.max(1) as f32;
        self.intervals.clear();
        self.costs.clear();
        self.window_frames = 0;
        self.window_misses = 0;
        let max_divisor = ((1000.0 / self.refresh_ms) / 50.0).floor().max(1.0) as u32;
        let before = self.divisor;
        // More than this share of frames late for their slot reads as judder.
        const MISS_LIMIT: f32 = 0.08;
        if missed > MISS_LIMIT && self.divisor < max_divisor {
            if self.trial > 0 {
                self.up_wait = (self.up_wait * 2).min(60);
            }
            self.divisor += 1;
            self.calm = 0;
            self.trial = 0;
        } else if self.divisor > 1 && p50 + 0.5 < (self.divisor - 1) as f32 * self.refresh_ms {
            // Frames measured at the slower pace run long (the processor
            // idles and slows between them), so a faster slot is tried when
            // the typical frame fits it; the misses decide whether it stays.
            self.calm += 1;
            if self.calm >= self.up_wait {
                self.divisor -= 1;
                self.calm = 0;
                self.trial = 2;
            }
        } else {
            self.calm = 0;
            self.trial = self.trial.saturating_sub(1);
        }
        if self.divisor != before {
            diag::info!(
                Launch,
                "frame pacing: every {} refresh(es) of {:.2} ms (frame p90 {:.1} ms)",
                self.divisor,
                self.refresh_ms,
                p90
            );
        }
    }
}

static FREE_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `?frames=free`: each frame starts as soon as the last one finished rather
/// than at the next display refresh. A frame that runs past a refresh then
/// costs only its overrun instead of waiting out a whole refresh interval.
pub fn set_free_running(on: bool) {
    FREE_RUNNING.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// How long `App::update` takes on the engine thread, main world and render
/// world together: the frame's whole CPU cost, which the refresh-paced frame
/// rate hides. One log line every 5 s.
#[derive(Default)]
struct FrameClock {
    window_start: Option<web_time::Instant>,
    frames: u32,
    update_ms: f64,
    worst_ms: f64,
    over_16: u32,
    /// Pipelined frames: game thread, render, and wait totals.
    split_frames: u32,
    game_ms: f64,
    render_ms: f64,
    wait_ms: f64,
}

impl FrameClock {
    /// A frame that started at `started` and cost `work_ms` (with a game
    /// thread, the slower of its frame and this thread's render).
    fn record(&mut self, started: web_time::Instant, work_ms: f32, split: Option<Split>) {
        let now = web_time::Instant::now();
        let ms = f64::from(work_ms);
        if let Some(split) = split {
            self.split_frames += 1;
            self.game_ms += f64::from(split.game_ms);
            self.render_ms += f64::from(split.render_ms);
            self.wait_ms += f64::from(split.wait_ms);
            if split.render_ms > 50.0 {
                diag::info!(Launch, "slow render: {:.0}ms", split.render_ms);
            }
        } else if ms > 50.0 {
            // What a long frame waited on: file reads that missed the page's
            // caches block this thread until the bytes arrive.
            let (read_ms, reads, bytes) = vfs::take_read_wait();
            diag::info!(
                Launch,
                "slow frame: {ms:.0}ms, of which file reads {read_ms:.0}ms ({reads} reads, {} KiB)",
                bytes / 1024
            );
        }
        self.frames += 1;
        self.update_ms += ms;
        self.worst_ms = self.worst_ms.max(ms);
        if ms > 16.0 {
            self.over_16 += 1;
        }
        let window_start = *self.window_start.get_or_insert(started);
        let elapsed = (now - window_start).as_secs_f64();
        if elapsed >= 5.0 {
            let gaps = PACER.with(|pacer| std::mem::take(&mut pacer.borrow_mut().gaps));
            let split = if self.split_frames > 0 {
                let n = f64::from(self.split_frames);
                format!(
                    " game avg={:.2}ms render avg={:.2}ms wait avg={:.2}ms",
                    self.game_ms / n,
                    self.render_ms / n,
                    self.wait_ms / n
                )
            } else {
                String::new()
            };
            diag::info!(
                Launch,
                "frame cpu: {elapsed:.1}s frames={} fps={:.1} update avg={:.2}ms worst={:.1}ms over16={} pacing={} gaps={}/{}/{}{split}",
                self.frames,
                f64::from(self.frames) / elapsed,
                self.update_ms / f64::from(self.frames),
                self.worst_ms,
                self.over_16,
                if FREE_RUNNING.load(std::sync::atomic::Ordering::Relaxed) {
                    "free".to_owned()
                } else {
                    format!("every {}", PACER.with(|pacer| pacer.borrow().divisor))
                },
                gaps[0],
                gaps[1],
                gaps[2],
            );
            *self = FrameClock::default();
        }
    }
}

/// What the page's automatic resolution reads once a second
/// (`iw4lFrameStats`, relayed by `web/engine-worker.js`): the frame rate and
/// the engine's update time, mean and 90th percentile. Frames that come late
/// while the update leaves time to spare wait on the graphics card.
#[derive(Default)]
struct PageFrameStats {
    window_start: Option<web_time::Instant>,
    update_ms: Vec<f32>,
}

impl PageFrameStats {
    fn record(&mut self, work_ms: f32) {
        let now = web_time::Instant::now();
        self.update_ms.push(work_ms);
        let window_start = *self.window_start.get_or_insert(now);
        let elapsed = (now - window_start).as_secs_f64();
        if elapsed < 1.0 {
            return;
        }
        let frames = self.update_ms.len();
        let mean = self.update_ms.iter().sum::<f32>() / frames as f32;
        self.update_ms.sort_unstable_by(f32::total_cmp);
        let p90 = self.update_ms[(frames * 9 / 10).min(frames - 1)];
        let fps = frames as f64 / elapsed;
        self.update_ms.clear();
        self.window_start = Some(now);
        use wasm_bindgen::JsCast;
        if let Ok(hook) = js_sys::Reflect::get(&js_sys::global(), &"iw4lFrameStats".into())
            && let Some(hook) = hook.dyn_ref::<js_sys::Function>()
        {
            let _ = hook.call3(
                &JsValue::NULL,
                &JsValue::from_f64(fps),
                &JsValue::from_f64(f64::from(mean)),
                &JsValue::from_f64(f64::from(p90)),
            );
        }
    }
}

/// Called by the worker entry (`iw4l_worker_main`) with the transferred
/// canvas and its size; `launch` then builds the app in worker mode.
pub fn install(canvas: JsValue, width: u32, height: u32, scale: f64) {
    HOST.with(|host| {
        *host.borrow_mut() = Some(Host {
            canvas,
            width: width.max(1),
            height: height.max(1),
            scale,
        });
    });
    ACTIVE.with(|active| active.set(true));
}

/// Whether the engine runs in a worker (the page thread owns the DOM).
pub fn active() -> bool {
    ACTIVE.with(Cell::get)
}

/// The transferred canvas as a window: wgpu's web backend reads the
/// `OffscreenCanvas` out of the raw handle's pointer to this `JsValue`.
struct OffscreenCanvasWindow {
    canvas: JsValue,
}

// SAFETY: the engine thread is the only one that ever touches the canvas
// handle; Bevy's window wrapper merely wants the type shareable.
unsafe impl Send for OffscreenCanvasWindow {}
unsafe impl Sync for OffscreenCanvasWindow {}

impl HasWindowHandle for OffscreenCanvasWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let pointer = NonNull::from(&self.canvas).cast();
        let handle = WebOffscreenCanvasWindowHandle::new(pointer);
        // SAFETY: the pointer stays valid for as long as this window, which
        // the wrapper keeps alive alongside the handle.
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::WebOffscreenCanvas(handle)) })
    }
}

impl HasDisplayHandle for OffscreenCanvasWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        // SAFETY: the web display handle carries no pointer.
        Ok(unsafe { DisplayHandle::borrow_raw(RawDisplayHandle::Web(WebDisplayHandle::new())) })
    }
}

// ---- input from the page ------------------------------------------------------

enum Input {
    Key {
        code: String,
        key: String,
        pressed: bool,
        repeat: bool,
    },
    Button {
        button: u32,
        pressed: bool,
    },
    Motion {
        dx: f32,
        dy: f32,
    },
    Cursor {
        x: f32,
        y: f32,
    },
    Wheel {
        dx: f32,
        dy: f32,
        lines: bool,
    },
    Resize {
        width: u32,
        height: u32,
        scale: f64,
    },
    Focus(bool),
    CursorEntered,
}

static INPUT: Mutex<Vec<Input>> = Mutex::new(Vec::new());

/// The worker script forwards each page event here (the launcher exports it
/// as `iw4l_worker_input`): `kind` 1 key (`s` is "code\tkey", `a` pressed,
/// `b` repeat), 2 mouse button (`a` button, `b` pressed), 3 mouse motion
/// (`a`,`b` delta), 4 cursor position in physical pixels, 5 wheel (`a`,`b`
/// delta, `c` 1 for lines), 6 resize (`a` width, `b` height in physical
/// pixels, `c` scale), 7 focus (`a`), 8 cursor entered.
pub fn input(kind: u32, a: f64, b: f64, c: f64, s: &str) {
    let input = match kind {
        1 => {
            let (code, key) = s.split_once('\t').unwrap_or((s, ""));
            Input::Key {
                code: code.to_owned(),
                key: key.to_owned(),
                pressed: a != 0.0,
                repeat: b != 0.0,
            }
        }
        2 => Input::Button {
            button: a as u32,
            pressed: b != 0.0,
        },
        3 => Input::Motion {
            dx: a as f32,
            dy: b as f32,
        },
        4 => Input::Cursor {
            x: a as f32,
            y: b as f32,
        },
        5 => Input::Wheel {
            dx: a as f32,
            dy: b as f32,
            lines: c != 0.0,
        },
        6 => Input::Resize {
            width: a as u32,
            height: b as u32,
            scale: c,
        },
        7 => Input::Focus(a != 0.0),
        8 => Input::CursorEntered,
        _ => return,
    };
    INPUT.lock().unwrap_or_else(|e| e.into_inner()).push(input);
}

#[wasm_bindgen]
extern "C" {
    /// The worker script relays these to the page.
    #[wasm_bindgen(catch, js_namespace = iw4lWorkerHost, js_name = cursor)]
    fn js_host_cursor(locked: bool, visible: bool) -> Result<(), JsValue>;
}

/// Primary window out of the transferred canvas, input from the page,
/// frames from the worker's animation clock.
pub struct WorkerWindowPlugin;

impl Plugin for WorkerWindowPlugin {
    fn build(&self, app: &mut App) {
        let host = HOST
            .with(|host| host.borrow_mut().take())
            .expect("worker window plugin without an installed canvas");
        let mut primary = app.world_mut().query_filtered::<Entity, With<PrimaryWindow>>();
        let entity = primary
            .single(app.world())
            .expect("the window plugin spawned a primary window");
        {
            let mut window = app
                .world_mut()
                .get_mut::<Window>(entity)
                .expect("primary window component");
            window.resolution.set_scale_factor(host.scale as f32);
            window
                .resolution
                .set_physical_resolution(host.width, host.height);
        }
        let wrapper = WindowWrapper::new(OffscreenCanvasWindow {
            canvas: host.canvas,
        });
        let handle = RawHandleWrapper::new(&wrapper).expect("offscreen canvas handle");
        app.world_mut().entity_mut(entity).insert(handle);
        diag::info!(
            Launch,
            "worker: primary window {}x{} scale {:.2} on the transferred canvas",
            host.width,
            host.height,
            host.scale
        );
        app.add_systems(
            PreUpdate,
            drain_page_input.before(bevy::input::InputSystems),
        )
        .add_systems(Last, forward_cursor_grab)
        .set_runner(worker_runner);
    }
}

#[derive(SystemParam)]
struct InputWriters<'w> {
    keys: MessageWriter<'w, KeyboardInput>,
    buttons: MessageWriter<'w, MouseButtonInput>,
    motion: MessageWriter<'w, MouseMotion>,
    cursor: MessageWriter<'w, CursorMoved>,
    wheel: MessageWriter<'w, MouseWheel>,
    resized: MessageWriter<'w, WindowResized>,
    focused: MessageWriter<'w, WindowFocused>,
    entered: MessageWriter<'w, CursorEntered>,
}

fn drain_page_input(
    mut windows: Query<(Entity, &mut Window), With<PrimaryWindow>>,
    mut out: InputWriters,
) {
    let Ok((window, mut primary)) = windows.single_mut() else {
        return;
    };
    let pending = std::mem::take(&mut *INPUT.lock().unwrap_or_else(|e| e.into_inner()));
    for input in pending {
        match input {
            Input::Key {
                code,
                key,
                pressed,
                repeat,
            } => {
                let logical = logical_key(&key);
                out.keys.write(KeyboardInput {
                    key_code: key_code_from_dom(&code),
                    text: key_text(&logical),
                    logical_key: logical,
                    state: if pressed {
                        ButtonState::Pressed
                    } else {
                        ButtonState::Released
                    },
                    repeat,
                    window,
                });
            }
            Input::Button { button, pressed } => {
                out.buttons.write(MouseButtonInput {
                    button: match button {
                        0 => MouseButton::Left,
                        1 => MouseButton::Middle,
                        2 => MouseButton::Right,
                        3 => MouseButton::Back,
                        4 => MouseButton::Forward,
                        other => MouseButton::Other(other as u16),
                    },
                    state: if pressed {
                        ButtonState::Pressed
                    } else {
                        ButtonState::Released
                    },
                    window,
                });
            }
            Input::Motion { dx, dy } => {
                out.motion.write(MouseMotion {
                    delta: Vec2::new(dx, dy),
                });
            }
            Input::Cursor { x, y } => {
                let position = Vec2::new(x, y);
                let scale = primary.resolution.scale_factor();
                let delta = primary
                    .physical_cursor_position()
                    .map(|previous| (position - previous) / scale);
                primary.set_physical_cursor_position(Some(position.as_dvec2()));
                out.cursor.write(CursorMoved {
                    window,
                    position: position / scale,
                    delta,
                });
            }
            Input::Wheel { dx, dy, lines } => {
                out.wheel.write(MouseWheel {
                    unit: if lines {
                        MouseScrollUnit::Line
                    } else {
                        MouseScrollUnit::Pixel
                    },
                    x: dx,
                    y: dy,
                    window,
                    phase: TouchPhase::Moved,
                });
            }
            Input::Resize {
                width,
                height,
                scale,
            } => {
                primary.resolution.set_scale_factor(scale as f32);
                primary
                    .resolution
                    .set_physical_resolution(width.max(1), height.max(1));
                out.resized.write(WindowResized {
                    window,
                    width: primary.width(),
                    height: primary.height(),
                });
            }
            Input::Focus(focused) => {
                primary.focused = focused;
                out.focused.write(WindowFocused { window, focused });
            }
            Input::CursorEntered => {
                out.entered.write(CursorEntered { window });
            }
        }
    }
}

/// The page holds the pointer; the engine says when it wants it locked.
fn forward_cursor_grab(
    cursors: Query<&CursorOptions, (With<PrimaryWindow>, Changed<CursorOptions>)>,
) {
    for cursor in &cursors {
        let locked = cursor.grab_mode != CursorGrabMode::None;
        let visible = cursor.visible;
        // The host hook lives on the engine worker; the game may run elsewhere.
        web_threads::on_engine(move || {
            if let Err(error) = js_host_cursor(locked, visible) {
                diag::warn!(Input, "worker: cursor request failed: {error:?}");
            }
        });
    }
}

// ---- frames ------------------------------------------------------------------

/// What the frame loop drives.
enum Engine {
    /// Plugins still finishing (the renderer waits for the GPU device).
    Starting(App),
    /// The game world and rendering in turn on this thread.
    Inline(App),
    /// The game world on a thread of its own, rendering here ([`pipeline`]).
    #[cfg(target_feature = "atomics")]
    Pipelined(pipeline::Pipeline),
    Stopped,
}

/// The worker's animation clock drives the app. Plugins that finish
/// asynchronously (the renderer waits for the adapter and device promises)
/// are polled until ready, as the winit runner does on the page.
fn worker_runner(app: App) -> AppExit {
    let engine = Rc::new(RefCell::new(Engine::Starting(app)));
    let launched = web_time::Instant::now();
    let frame: Rc<RefCell<Option<Closure<dyn FnMut(JsValue)>>>> = Rc::new(RefCell::new(None));
    let again = frame.clone();
    // An animation frame passes its refresh timestamp; a message or timer
    // (free-running, no animation clock) passes something else.
    *frame.borrow_mut() = Some(Closure::new(move |stamp: JsValue| {
        let stamp = stamp.as_f64();
        // Threads asked for from other threads start here, where the
        // spawner lives; page calls they left run here too.
        #[cfg(target_feature = "atomics")]
        {
            web_threads::web::drain_pending();
            web_threads::web::run_engine_jobs();
        }
        let exit = frame_tick(&mut engine.borrow_mut(), stamp, launched);
        if let Some(exit) = exit {
            diag::info!(Launch, "worker: app exit {exit:?}");
            return;
        }
        if let Some(closure) = again.borrow().as_ref() {
            request_frame(closure);
        }
    }));
    if let Some(closure) = frame.borrow().as_ref() {
        request_frame(closure);
    }
    AppExit::Success
}

fn frame_tick(engine: &mut Engine, stamp: Option<f64>, launched: web_time::Instant) -> Option<AppExit> {
    match engine {
        Engine::Starting(app) => {
            match app.plugins_state() {
                PluginsState::Adding => return None,
                PluginsState::Ready => {
                    app.finish();
                    app.cleanup();
                    diag::info!(
                        Launch,
                        "worker: plugins ready after {} ms",
                        launched.elapsed().as_millis()
                    );
                }
                PluginsState::Finished | PluginsState::Cleaned => {}
            }
            let Engine::Starting(app) = std::mem::replace(engine, Engine::Stopped) else {
                unreachable!("matched above");
            };
            *engine = start_engine(app);
            None
        }
        Engine::Inline(app) => {
            let started = web_time::Instant::now();
            if !PACER.with(|pacer| pacer.borrow_mut().tick(stamp)) {
                // Between paced frames: the next refresh.
                return None;
            }
            let _ = vfs::take_read_wait();
            app.update();
            let work = started.elapsed().as_secs_f32() * 1000.0;
            record_frame(stamp, started, work, None);
            app.should_exit()
        }
        #[cfg(target_feature = "atomics")]
        Engine::Pipelined(pipeline) => {
            let started = web_time::Instant::now();
            if !PACER.with(|pacer| pacer.borrow_mut().tick(stamp)) {
                return None;
            }
            // The game thread gets this long to finish its frame before this
            // refresh is given up and the page's events run.
            let tick = pipeline.tick(std::time::Duration::from_millis(20));
            if tick.exit.is_some() {
                return tick.exit;
            }
            if let Some(split) = tick.drawn {
                // The frame takes as long as the slower of the two threads.
                let work = split.game_ms.max(split.render_ms);
                record_frame(stamp, started, work, Some(split));
            }
            None
        }
        Engine::Stopped => None,
    }
}

/// The engine once its plugins are ready: pipelined where it can be.
fn start_engine(mut app: App) -> Engine {
    crate::bench::facts::summarize_adapter(app.world_mut());
    #[cfg(target_feature = "atomics")]
    let app = if PIPELINE.load(std::sync::atomic::Ordering::Relaxed) {
        match pipeline::Pipeline::start(app) {
            Ok(pipelined) => {
                diag::info!(
                    Launch,
                    "frames: the game world runs on its own thread, rendering on this one"
                );
                return Engine::Pipelined(pipelined);
            }
            Err((app, why)) => {
                diag::info!(Launch, "frames: game world and rendering in turn ({why})");
                app
            }
        }
    } else {
        diag::info!(Launch, "frames: game world and rendering in turn (pipeline=0)");
        app
    };
    Engine::Inline(app)
}

fn record_frame(stamp: Option<f64>, started: web_time::Instant, work_ms: f32, split: Option<Split>) {
    FRAME_CLOCK.with(|clock| clock.borrow_mut().record(started, work_ms, split));
    PAGE_STATS.with(|stats| stats.borrow_mut().record(work_ms));
    PACER.with(|pacer| pacer.borrow_mut().ran(stamp, started, work_ms));
}

/// Where a pipelined frame's time went, in milliseconds: the game thread's
/// frame, this thread's extract and render, and its wait for the game.
#[derive(Clone, Copy, Default)]
struct Split {
    game_ms: f32,
    render_ms: f32,
    wait_ms: f32,
}

static PIPELINE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// `?pipeline=0`: the game world and rendering in turn on the engine thread,
/// as before the game thread.
pub fn set_pipeline(on: bool) {
    PIPELINE.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// The game world on a thread of its own while this thread renders.
///
/// Bevy pipelines the same way on the desktop: the main world updates frame
/// N+1 while the render world draws frame N, and they meet at extract. In the
/// browser the render world must stay on this thread, which owns the GPU
/// device and the canvas, so it is the main world that moves: to a resident
/// thread (`web_threads::web::spawn_resident`) whose event loop keeps the
/// futures, timers and sockets the game starts. Page calls the game makes
/// come back here (`web_threads::on_engine`).
///
/// Each frame: wait for the game's frame, extract it, start the game's next
/// frame, render. A frame then costs the slower of the two halves rather
/// than their sum, for one more frame between input and the screen.
#[cfg(target_feature = "atomics")]
mod pipeline {
    use std::cell::UnsafeCell;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use bevy::app::SubApp;
    use bevy::prelude::*;
    use bevy::render::RenderApp;
    use web_threads::web::{Resident, Waited};

    use super::Split;

    /// The app, render world taken out, shared by this thread and the game
    /// thread: one of them at a time, handed over by the step protocol.
    struct GameApp {
        app: UnsafeCell<App>,
        /// The game thread's last frame, in microseconds.
        frame_us: AtomicU32,
    }

    // SAFETY: the game thread uses the app only inside a step (between a
    // `kick` and the step finishing), this thread only between `wait`
    // reporting the step done and the next `kick`; the step's lock and
    // atomics order the two. Nothing in the main world is tied to a thread
    // (no non-send resources), and calls into the page go through
    // `web_threads::on_engine`.
    unsafe impl Send for GameApp {}
    unsafe impl Sync for GameApp {}

    pub(super) struct Pipeline {
        game: Arc<GameApp>,
        render: SubApp,
        thread: Resident,
        /// A game frame was asked for and not yet waited for.
        in_flight: bool,
        /// A game frame has run, so there is one to draw.
        stepped: bool,
        /// Waited in refreshes given up while the game frame ran on.
        waited_ms: f32,
    }

    pub(super) struct Tick {
        pub(super) exit: Option<AppExit>,
        /// Set when a frame was drawn.
        pub(super) drawn: Option<Split>,
    }

    impl Pipeline {
        /// Moves `app`'s main world to a game thread. `Err` gives the app
        /// back, with why, when it cannot be split.
        pub(super) fn start(mut app: App) -> Result<Self, (App, &'static str)> {
            if !web_threads::web::supported() {
                return Err((app, "no threads"));
            }
            let Some(render) = app.remove_sub_app(RenderApp) else {
                return Err((app, "no render world"));
            };
            if !app.sub_apps().sub_apps.is_empty() {
                app.insert_sub_app(RenderApp, render);
                return Err((app, "other sub-apps"));
            }
            // GPU handles may not be touched off this thread; the render
            // world keeps its own. A main-world system that still asks for
            // one now finds it missing rather than crashing the game thread.
            {
                use bevy::render::renderer::{
                    RenderAdapter, RenderAdapterInfo, RenderDevice, RenderInstance, RenderQueue,
                };
                let world = app.world_mut();
                world.remove_resource::<RenderDevice>();
                world.remove_resource::<RenderQueue>();
                world.remove_resource::<RenderAdapter>();
                world.remove_resource::<RenderAdapterInfo>();
                world.remove_resource::<RenderInstance>();
            }
            let game = Arc::new(GameApp {
                app: UnsafeCell::new(app),
                frame_us: AtomicU32::new(0),
            });
            let stepping = game.clone();
            let thread = web_threads::web::spawn_resident("iw4l game", move || {
                game_frame(&stepping);
            });
            match thread {
                Ok(thread) => Ok(Self {
                    game,
                    render,
                    thread,
                    in_flight: false,
                    stepped: false,
                    waited_ms: 0.0,
                }),
                Err(_) => {
                    // The thread never started, so its step (and its share
                    // of the app) is gone again.
                    let Ok(game) = Arc::try_unwrap(game) else {
                        unreachable!("the step was dropped with the thread");
                    };
                    let mut app = game.app.into_inner();
                    app.insert_sub_app(RenderApp, render);
                    Err((app, "the game thread did not start"))
                }
            }
        }

        /// One refresh: when the game's frame is done within `budget`,
        /// extract it, start the next and render.
        pub(super) fn tick(&mut self, budget: Duration) -> Tick {
            let waiting = web_time::Instant::now();
            if self.in_flight {
                match self.thread.wait(budget) {
                    Waited::Done => self.in_flight = false,
                    Waited::Busy => {
                        // Long game frames (a map load): this refresh goes,
                        // and the page's events run before the next.
                        self.waited_ms += waiting.elapsed().as_secs_f32() * 1000.0;
                        return Tick {
                            exit: None,
                            drawn: None,
                        };
                    }
                    Waited::Dead(said) => panic!("the game thread stopped: {said}"),
                }
            }
            let wait_ms = self.waited_ms + waiting.elapsed().as_secs_f32() * 1000.0;
            self.waited_ms = 0.0;
            // SAFETY: no step runs: the last one finished above and the next
            // is not asked for until after this borrow's last use.
            let app = unsafe { &mut *self.game.app.get() };
            if let Some(exit) = app.should_exit() {
                return Tick {
                    exit: Some(exit),
                    drawn: None,
                };
            }
            let rendering = web_time::Instant::now();
            let draw = self.stepped;
            if draw {
                let world = app.main_mut().world_mut();
                self.render.extract(world);
                world.clear_trackers();
            }
            self.thread.kick();
            self.in_flight = true;
            self.stepped = true;
            if !draw {
                return Tick {
                    exit: None,
                    drawn: None,
                };
            }
            self.render.update();
            Tick {
                exit: None,
                drawn: Some(Split {
                    game_ms: self.game.frame_us.load(Ordering::Relaxed) as f32 / 1000.0,
                    render_ms: rendering.elapsed().as_secs_f32() * 1000.0,
                    wait_ms,
                }),
            }
        }
    }

    /// One frame of the main world, on the game thread.
    fn game_frame(game: &GameApp) {
        // SAFETY: see `GameApp`: inside a step, the app is this thread's.
        let app = unsafe { &mut *game.app.get() };
        let started = web_time::Instant::now();
        let _ = vfs::take_read_wait();
        app.main_mut().run_default_schedule();
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        game.frame_us.store((ms * 1000.0) as u32, Ordering::Relaxed);
        if ms > 50.0 {
            // What a long frame waited on: file reads that missed the page's
            // caches block this thread until the bytes arrive.
            let (read_ms, reads, bytes) = vfs::take_read_wait();
            diag::info!(
                Launch,
                "slow game frame: {ms:.0}ms, of which file reads {read_ms:.0}ms ({reads} reads, {} KiB)",
                bytes / 1024
            );
        }
    }
}

fn request_frame(closure: &Closure<dyn FnMut(JsValue)>) {
    if FREE_RUNNING.load(std::sync::atomic::Ordering::Relaxed) {
        // A message to ourselves: a task of its own, so input and the read
        // worker's replies still run between frames, without the 4 ms clamp
        // nested timers get.
        let posted = NEXT_FRAME.with(|channel| {
            let mut channel = channel.borrow_mut();
            if channel.is_none() {
                let created = web_sys::MessageChannel::new().ok()?;
                created
                    .port1()
                    .set_onmessage(Some(closure.as_ref().unchecked_ref()));
                *channel = Some(created);
            }
            channel.as_ref()?.port2().post_message(&JsValue::NULL).ok()
        });
        if posted.is_some() {
            return;
        }
        diag::warn!(Launch, "worker: free-running frames unavailable; pacing by refresh");
        FREE_RUNNING.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    let global = js_sys::global();
    let has_raf = js_sys::Reflect::has(&global, &"requestAnimationFrame".into()).unwrap_or(false);
    let result = if has_raf {
        global
            .unchecked_ref::<web_sys::DedicatedWorkerGlobalScope>()
            .request_animation_frame(closure.as_ref().unchecked_ref())
            .map(|_| ())
    } else {
        // A worker without an animation clock (older Firefox): pace by timer.
        global
            .unchecked_ref::<web_sys::WorkerGlobalScope>()
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                closure.as_ref().unchecked_ref(),
                16,
            )
            .map(|_| ())
    };
    if let Err(error) = result {
        diag::warn!(Launch, "worker: frame request failed: {error:?}");
    }
}

// ---- keys --------------------------------------------------------------------

/// DOM `KeyboardEvent.key` to Bevy's logical key.
fn logical_key(key: &str) -> Key {
    let mut chars = key.chars();
    if let (Some(single), None) = (chars.next(), chars.next()) {
        return if single == ' ' {
            Key::Space
        } else {
            Key::Character(single.to_string().into())
        };
    }
    match key {
        "Enter" => Key::Enter,
        "Tab" => Key::Tab,
        "Backspace" => Key::Backspace,
        "Delete" => Key::Delete,
        "Escape" => Key::Escape,
        "Shift" => Key::Shift,
        "Control" => Key::Control,
        "Alt" => Key::Alt,
        "AltGraph" => Key::AltGraph,
        "Meta" => Key::Meta,
        "CapsLock" => Key::CapsLock,
        "NumLock" => Key::NumLock,
        "ScrollLock" => Key::ScrollLock,
        "ArrowUp" => Key::ArrowUp,
        "ArrowDown" => Key::ArrowDown,
        "ArrowLeft" => Key::ArrowLeft,
        "ArrowRight" => Key::ArrowRight,
        "Home" => Key::Home,
        "End" => Key::End,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        "Insert" => Key::Insert,
        "ContextMenu" => Key::ContextMenu,
        "PrintScreen" => Key::PrintScreen,
        "Pause" => Key::Pause,
        "F1" => Key::F1,
        "F2" => Key::F2,
        "F3" => Key::F3,
        "F4" => Key::F4,
        "F5" => Key::F5,
        "F6" => Key::F6,
        "F7" => Key::F7,
        "F8" => Key::F8,
        "F9" => Key::F9,
        "F10" => Key::F10,
        "F11" => Key::F11,
        "F12" => Key::F12,
        "Dead" => Key::Dead(None),
        _ => Key::Unidentified(NativeKey::Unidentified),
    }
}

/// What winit reports as the key's text: the character, or the control
/// character for the few named keys that have one.
fn key_text(key: &Key) -> Option<smol_str::SmolStr> {
    match key {
        Key::Character(text) => Some(text.clone()),
        Key::Space => Some(" ".into()),
        Key::Enter => Some("\r".into()),
        Key::Backspace => Some("\u{8}".into()),
        Key::Tab => Some("\t".into()),
        Key::Escape => Some("\u{1b}".into()),
        _ => None,
    }
}

include!("web_worker_keys.rs");
