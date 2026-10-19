//! Streamed clips decoded by the browser instead of on the page thread.
//!
//! MW2's streamed sounds are MP3s. Decoding one in the engine takes tens of
//! milliseconds of the page thread, and a burst of new sounds (a firefight,
//! a weapon swap) stalled frames for hundreds. `decodeAudioData` runs on the
//! browser's own audio decoder thread and hands back PCM at the output
//! rate; the clip job waits in line until it is there.
//!
//! The decoder is the engine worker's (`iw4lDecode`, `web/engine-worker.js`).
//! Clips submitted on another thread (the game thread) go through a broker
//! the engine thread services once a frame.

use std::sync::Arc;

use wasm_bindgen::prelude::*;

use crate::clip_store::ClipError;
use crate::media::PcmBuffer;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(catch, js_namespace = iw4lDecode, js_name = submit)]
    fn js_submit(bytes: &[u8]) -> Result<u32, JsValue>;
    /// 0 pending, 1 ready, -1 failed.
    #[wasm_bindgen(js_namespace = iw4lDecode, js_name = state)]
    fn js_state(id: u32) -> i32;
    #[wasm_bindgen(js_namespace = iw4lDecode, js_name = channels)]
    fn js_channels(id: u32) -> u32;
    #[wasm_bindgen(js_namespace = iw4lDecode, js_name = rate)]
    fn js_rate(id: u32) -> u32;
    /// Interleaved samples; the job is forgotten by the page afterwards.
    #[wasm_bindgen(js_namespace = iw4lDecode, js_name = take)]
    fn js_take(id: u32) -> Vec<f32>;
    #[wasm_bindgen(js_namespace = iw4lDecode, js_name = error)]
    fn js_error(id: u32) -> String;
}

pub(crate) enum BrowserDecode {
    Pending,
    Done(Result<PcmBuffer, ClipError>),
}

/// Hand `bytes` to the browser; `None` when the page has no decoder (an
/// older page), in which case the engine decodes inline as before.
pub(crate) fn submit(bytes: &[u8]) -> Option<u32> {
    #[cfg(target_feature = "atomics")]
    if !web_threads::on_device_thread() {
        return Some(broker::submit(bytes));
    }
    js_submit(bytes).ok()
}

pub(crate) fn poll(id: u32) -> BrowserDecode {
    #[cfg(target_feature = "atomics")]
    if id & broker::BROKERED != 0 {
        return broker::poll(id);
    }
    poll_decoder(id)
}

/// The engine worker's decoder's answer for job `id`.
fn poll_decoder(id: u32) -> BrowserDecode {
    match js_state(id) {
        0 => BrowserDecode::Pending,
        1 => {
            let channels = js_channels(id);
            let rate = js_rate(id);
            let samples: Arc<[f32]> = js_take(id).into();
            let channels = u16::try_from(channels).unwrap_or(2);
            BrowserDecode::Done(PcmBuffer::from_pcm(samples, channels, rate).map_err(ClipError::InvalidPcm))
        }
        _ => {
            let error = js_error(id);
            diag::warn!(Audio, "audio: browser decode failed: {error}");
            BrowserDecode::Done(Err(ClipError::Decode))
        }
    }
}

/// Decodes asked for off the engine thread, handed to the decoder there.
#[cfg(target_feature = "atomics")]
mod broker {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::{BrowserDecode, js_submit, poll_decoder};
    use crate::clip_store::ClipError;

    /// Marks a brokered job's id (the decoder's own ids stay below it).
    pub(super) const BROKERED: u32 = 1 << 31;

    enum Job {
        Waiting(Vec<u8>),
        Decoding(u32),
        Finished(BrowserDecode),
    }

    static JOBS: Mutex<Option<HashMap<u32, Job>>> = Mutex::new(None);
    static NEXT: AtomicU32 = AtomicU32::new(1);

    pub(super) fn submit(bytes: &[u8]) -> u32 {
        web_threads::web::on_engine_frame("audio decode", service);
        let id = BROKERED | (NEXT.fetch_add(1, Ordering::Relaxed) & !BROKERED);
        JOBS.lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(HashMap::new)
            .insert(id, Job::Waiting(bytes.to_vec()));
        id
    }

    pub(super) fn poll(id: u32) -> BrowserDecode {
        let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
        let jobs = jobs.get_or_insert_with(HashMap::new);
        match jobs.remove(&id) {
            Some(Job::Finished(result)) => result,
            Some(job) => {
                jobs.insert(id, job);
                BrowserDecode::Pending
            }
            None => BrowserDecode::Done(Err(ClipError::Decode)),
        }
    }

    /// The engine thread, once a frame: submits what waits, collects what
    /// the decoder finished.
    fn service() {
        let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(jobs) = jobs.as_mut() else {
            return;
        };
        for job in jobs.values_mut() {
            match job {
                Job::Waiting(bytes) => {
                    *job = match js_submit(bytes) {
                        Ok(id) => Job::Decoding(id),
                        Err(error) => {
                            diag::warn!(Audio, "audio: browser decode refused: {error:?}");
                            Job::Finished(BrowserDecode::Done(Err(ClipError::Decode)))
                        }
                    };
                }
                Job::Decoding(id) => {
                    let result = poll_decoder(*id);
                    if !matches!(result, BrowserDecode::Pending) {
                        *job = Job::Finished(result);
                    }
                }
                Job::Finished(_) => {}
            }
        }
    }
}
