//! The browser's output: an `AudioWorkletNode` reading a shared ring buffer
//! that the engine keeps filled.
//!
//! cpal's Web Audio host schedules two buffers from page-thread timers, so
//! its lookahead was two buffers and every game frame longer than that was
//! a gap (5 ms buffers stuttered on every frame; 43 ms ones added latency).
//! Here the worklet thread pulls 128-frame quanta straight out of a
//! `SharedArrayBuffer` ring, and [`WebOutput::pump`] renders ahead once a
//! frame. The ring holds ~170 ms, so a frame has to stall that long before
//! the worklet runs dry; the sound itself plays as soon as the worklet
//! reaches it, not when a timer fires.
//!
//! On the page the engine makes the context and the ring itself. In a Web
//! Worker there is no `AudioContext`: the page makes both and publishes the
//! ring as `globalThis.iw4lAudioRing` (`web/engine-worker.js`), and the
//! engine only fills it.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use js_sys::{Atomics, Float32Array, Int32Array, Object, Reflect, SharedArrayBuffer};
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{
    AudioContext, AudioContextOptions, AudioWorkletNode, AudioWorkletNodeOptions,
};

use crate::render_core::{QUANTUM, RenderShared, SAMPLE_RATE};

/// Frames of stereo audio the ring holds: 64 quanta, ~170 ms at 48 kHz.
const RING_FRAMES: usize = QUANTUM * 64;
const CHANNELS: usize = 2;

/// Control words shared with the worklet: frames written, frames read
/// (both wrapping `i32` counters), underruns the worklet filled with
/// silence, and (page-made rings) 1 once the page attached the node, -1 when
/// it could not.
const CTL_WRITTEN: u32 = 0;
const CTL_READ: u32 = 1;
const CTL_UNDERRUNS: u32 = 2;
const CTL_ATTACHED: u32 = 3;

/// Served next to the page; registers the `iw4l-output` processor.
const WORKLET_MODULE: &str = "audio-worklet.js";

pub(crate) struct WebOutput {
    shared: Arc<RenderShared>,
    /// The engine's own context; `None` when the page made the ring.
    context: Option<AudioContext>,
    rate: f32,
    frames: usize,
    ring: Float32Array,
    control: Int32Array,
    /// Set once the worklet module has loaded and the node is connected.
    node: Rc<Cell<Option<AudioWorkletNode>>>,
    failed: Rc<Cell<Option<String>>>,
    written: i32,
    underruns_seen: i32,
    block: Box<[[f32; 2]; QUANTUM]>,
    staging: Vec<f32>,
}

/// A ring the page made for an engine running in a worker.
struct PageRing {
    ring: SharedArrayBuffer,
    control: SharedArrayBuffer,
    frames: usize,
    rate: f32,
}

fn page_ring() -> Option<PageRing> {
    let global = js_sys::global();
    let published = Reflect::get(&global, &"iw4lAudioRing".into()).ok()?;
    if published.is_undefined() || published.is_null() {
        return None;
    }
    let field = |name: &str| Reflect::get(&published, &name.into()).ok();
    let ring = field("ring")?.dyn_into::<SharedArrayBuffer>().ok()?;
    let control = field("control")?.dyn_into::<SharedArrayBuffer>().ok()?;
    let frames = field("frames")?.as_f64()? as usize;
    let rate = field("rate")?.as_f64()? as f32;
    (frames >= QUANTUM * 2).then_some(PageRing {
        ring,
        control,
        frames,
        rate,
    })
}

impl WebOutput {
    /// Create the context and start loading the worklet; the ring fills from
    /// the first `pump`, the node attaches when the module has loaded.
    pub(crate) fn open(shared: Arc<RenderShared>) -> Result<Self, String> {
        if !js_sys::global()
            .dyn_ref::<Object>()
            .and_then(|global| Reflect::get(global, &"crossOriginIsolated".into()).ok())
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            return Err("page is not cross-origin isolated; no SharedArrayBuffer".into());
        }
        let node: Rc<Cell<Option<AudioWorkletNode>>> = Rc::new(Cell::new(None));
        let failed: Rc<Cell<Option<String>>> = Rc::new(Cell::new(None));

        if let Some(page) = page_ring() {
            diag::info!(
                Audio,
                "audio: using the page's worklet ring ({} frames)",
                page.frames
            );
            return Ok(Self {
                shared,
                context: None,
                rate: page.rate,
                frames: page.frames,
                ring: Float32Array::new(&page.ring),
                control: Int32Array::new(&page.control),
                node,
                failed,
                written: 0,
                underruns_seen: 0,
                block: Box::new([[0.0; 2]; QUANTUM]),
                staging: vec![0.0; QUANTUM * CHANNELS],
            });
        }

        let options = AudioContextOptions::new();
        options.set_sample_rate(SAMPLE_RATE as f32);
        let context = AudioContext::new_with_context_options(&options)
            .map_err(|error| format!("AudioContext: {error:?}"))?;
        let ring_buffer = SharedArrayBuffer::new((RING_FRAMES * CHANNELS * 4) as u32);
        let control_buffer = SharedArrayBuffer::new(4 * 4);
        let ring = Float32Array::new(&ring_buffer);
        let control = Int32Array::new(&control_buffer);

        let worklet = context
            .audio_worklet()
            .map_err(|error| format!("AudioWorklet: {error:?}"))?;
        let loading = worklet
            .add_module(WORKLET_MODULE)
            .map_err(|error| format!("addModule: {error:?}"))?;
        let attach_context = context.clone();
        let attach_node = node.clone();
        let attach_failed = failed.clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(error) = wasm_bindgen_futures::JsFuture::from(loading).await {
                attach_failed.set(Some(format!("worklet module: {error:?}")));
                return;
            }
            match attach(&attach_context, &ring_buffer, &control_buffer) {
                Ok(node) => attach_node.set(Some(node)),
                Err(error) => attach_failed.set(Some(error)),
            }
        });
        let rate = context.sample_rate();
        Ok(Self {
            shared,
            context: Some(context),
            rate,
            frames: RING_FRAMES,
            ring,
            control,
            node,
            failed,
            written: 0,
            underruns_seen: 0,
            block: Box::new([[0.0; 2]; QUANTUM]),
            staging: vec![0.0; QUANTUM * CHANNELS],
        })
    }

    /// Whether the worklet failed to come up; the runtime then stops asking.
    pub(crate) fn failure(&self) -> Option<String> {
        if self.context.is_none()
            && Atomics::load(&self.control, CTL_ATTACHED).unwrap_or(0) < 0
        {
            return Some("the page could not attach its worklet".into());
        }
        let failed = self.failed.take();
        self.failed.set(failed.clone());
        failed
    }

    /// Render ahead until the ring is full. Called once a frame.
    pub(crate) fn pump(&mut self) {
        let read = Atomics::load(&self.control, CTL_READ).unwrap_or(0);
        let mut filled = self.written.wrapping_sub(read).max(0) as usize;
        let mut rendered = 0;
        while filled + QUANTUM <= self.frames {
            self.shared.render_for(&mut self.block, Some(true));
            for (frame, sample) in self.block.iter().zip(self.staging.chunks_exact_mut(CHANNELS)) {
                sample[0] = frame[0];
                sample[1] = frame[1];
            }
            let at = (self.written.rem_euclid(self.frames as i32) as usize) * CHANNELS;
            self.ring
                .subarray(at as u32, (at + QUANTUM * CHANNELS) as u32)
                .copy_from(&self.staging);
            self.written = self.written.wrapping_add(QUANTUM as i32);
            let _ = Atomics::store(&self.control, CTL_WRITTEN, self.written);
            filled += QUANTUM;
            rendered += 1;
        }
        if rendered > 0 {
            self.shared
                .device_callbacks
                .fetch_add(1, Ordering::Release);
            self.shared
                .web_quanta
                .fetch_add(rendered, Ordering::Relaxed);
        }
        let underruns = Atomics::load(&self.control, CTL_UNDERRUNS).unwrap_or(0);
        let fresh = underruns.wrapping_sub(self.underruns_seen);
        if fresh > 0 {
            self.shared
                .device_underruns
                .fetch_add(fresh as u64, Ordering::Relaxed);
            self.underruns_seen = underruns;
        }
    }

    pub(crate) fn attached(&self) -> bool {
        if self.context.is_none() {
            return Atomics::load(&self.control, CTL_ATTACHED).unwrap_or(0) > 0;
        }
        let node = self.node.take();
        let attached = node.is_some();
        self.node.set(node);
        attached
    }

    pub(crate) fn sample_rate(&self) -> f32 {
        self.rate
    }
}

fn attach(
    context: &AudioContext,
    ring: &SharedArrayBuffer,
    control: &SharedArrayBuffer,
) -> Result<AudioWorkletNode, String> {
    let processor_options = Object::new();
    let set = |key: &str, value: JsValue| {
        Reflect::set(&processor_options, &key.into(), &value)
            .map(|_| ())
            .map_err(|error| format!("processorOptions.{key}: {error:?}"))
    };
    set("ring", ring.into())?;
    set("control", control.into())?;
    set("frames", JsValue::from_f64(RING_FRAMES as f64))?;
    let options = AudioWorkletNodeOptions::new();
    options.set_number_of_inputs(0);
    options.set_number_of_outputs(1);
    options.set_output_channel_count(&js_sys::Array::of1(&JsValue::from_f64(CHANNELS as f64)));
    options.set_processor_options(Some(&processor_options));
    let node = AudioWorkletNode::new_with_options(context, "iw4l-output", &options)
        .map_err(|error| format!("AudioWorkletNode: {error:?}"))?;
    node.connect_with_audio_node(&context.destination())
        .map_err(|error| format!("connect: {error:?}"))?;
    Ok(node)
}
