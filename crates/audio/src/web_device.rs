//! The threaded browser build's output thread: keeps the page's worklet
//! ring full on its own, so an engine frame that runs long (a load stage, a
//! hitch) no longer starves the worklet. The single-threaded build fills the
//! ring once per frame from `AudioRuntime::pump` instead.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::render_core::RenderShared;
use crate::web_output::WebOutput;

/// Between pumps; the ring holds ~170 ms and a quantum is 2.7 ms.
const PUMP_EVERY: Duration = Duration::from_millis(2);

pub(crate) fn run(shared: Arc<RenderShared>, shutdown: Arc<AtomicBool>) {
    let mut output = match WebOutput::open(shared.clone()) {
        Ok(output) => output,
        Err(error) => {
            shared.device_required.store(false, Ordering::Relaxed);
            diag::warn!(Audio, "audio: browser output unavailable on its thread: {error}");
            return;
        }
    };
    diag::info!(
        Audio,
        "audio: browser output on its own thread: worklet ring at {} Hz",
        output.sample_rate()
    );
    let mut active = false;
    // Gaps between pumps longer than the ring can cover are what starve the
    // worklet; say where the time went, a few times at most.
    let mut last_pump = web_time::Instant::now();
    let mut long_gaps = 0u32;
    let mut long_pumps = 0u32;
    while !shutdown.load(Ordering::Acquire) {
        if let Some(error) = output.failure() {
            shared.device_required.store(false, Ordering::Relaxed);
            shared.device_active.store(false, Ordering::Release);
            diag::warn!(Audio, "audio: browser output failed: {error}");
            return;
        }
        let mixed_at = web_time::Instant::now();
        let gap = mixed_at.duration_since(last_pump);
        if gap > Duration::from_millis(60) {
            long_gaps += 1;
            if long_gaps <= 8 {
                diag::warn!(Audio, "audio: output thread was away {} ms between pumps", gap.as_millis());
            }
        }
        output.pump();
        let took = mixed_at.elapsed();
        last_pump = web_time::Instant::now();
        if took > Duration::from_millis(60) {
            long_pumps += 1;
            if long_pumps <= 8 {
                diag::warn!(Audio, "audio: one pump took {} ms", took.as_millis());
            }
        }
        shared
            .web_mix_ns
            .fetch_add(took.as_nanos() as u64, Ordering::Relaxed);
        if !active && output.attached() {
            active = true;
            shared.device_active.store(true, Ordering::Release);
            diag::info!(Audio, "audio: output running: worklet attached, fed from the output thread");
        }
        std::thread::sleep(PUMP_EVERY);
    }
    shared.device_active.store(false, Ordering::Release);
}
