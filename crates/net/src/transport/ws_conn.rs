//! The browser's stand-in for the relay's QUIC connection: one WebSocket to
//! the LAN server (`iw4l-master lan`), which opens the QUIC connection on the
//! page's behalf. Each binary message is one lane byte and that lane's bytes:
//!
//! * control: the single bidirectional stream, as a byte stream;
//! * datagram: one QUIC datagram;
//! * uni: the whole contents of one unidirectional stream.
//!
//! The method names mirror the `quinn` calls the bridge makes, so
//! `master.rs` drives either with the same code.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fmt;
use std::rc::Rc;
use std::task::{Poll, Waker};
use std::time::Duration;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{BinaryType, CloseEvent, MessageEvent, WebSocket};
use web_time::Instant;

const LANE_CONTROL: u8 = 0;
const LANE_DATAGRAM: u8 = 1;
const LANE_UNI: u8 = 2;

/// Unread datagrams beyond this are dropped oldest first, as a full QUIC
/// receive buffer would.
const MAX_QUEUED_DATAGRAMS: usize = 1024;

#[derive(Clone, Debug)]
pub struct WsError(String);

impl fmt::Display for WsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for WsError {}

#[derive(Default)]
struct Lanes {
    open: bool,
    closed: Option<String>,
    control: VecDeque<u8>,
    datagrams: VecDeque<Vec<u8>>,
    unis: VecDeque<Vec<u8>>,
    wakers: Vec<Waker>,
    /// Messages per lane, for the bridge's traffic log.
    sent: [u64; 3],
    received: [u64; 3],
    dropped_datagrams: u64,
}

type Shared = Rc<RefCell<Lanes>>;

fn wake(lanes: &Shared) {
    let wakers = std::mem::take(&mut lanes.borrow_mut().wakers);
    for waker in wakers {
        waker.wake();
    }
}

fn close_with(lanes: &Shared, reason: String) {
    {
        let mut lanes = lanes.borrow_mut();
        if lanes.closed.is_none() {
            lanes.closed = Some(reason);
        }
    }
    wake(lanes);
}

/// Resolves with `take`'s value, or the close reason once the socket closes.
async fn wait<T>(lanes: &Shared, mut take: impl FnMut(&mut Lanes) -> Option<T>) -> Result<T, WsError> {
    std::future::poll_fn(|cx| {
        let mut state = lanes.borrow_mut();
        if let Some(value) = take(&mut state) {
            return Poll::Ready(Ok(value));
        }
        if let Some(reason) = &state.closed {
            return Poll::Ready(Err(WsError(reason.clone())));
        }
        state.wakers.push(cx.waker().clone());
        Poll::Pending
    })
    .await
}

struct Inner {
    socket: WebSocket,
    lanes: Shared,
    rtt: Cell<Duration>,
    _callbacks: (
        Closure<dyn FnMut(JsValue)>,
        Closure<dyn FnMut(MessageEvent)>,
        Closure<dyn FnMut(CloseEvent)>,
        Closure<dyn FnMut(JsValue)>,
    ),
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.socket.set_onopen(None);
        self.socket.set_onmessage(None);
        self.socket.set_onclose(None);
        self.socket.set_onerror(None);
        let _ = self.socket.close();
    }
}

#[derive(Clone)]
pub struct WsConnection(Rc<Inner>);

impl WsConnection {
    /// Opens `url` and resolves once the socket is open.
    pub async fn connect(url: &str) -> Result<Self, WsError> {
        let socket =
            WebSocket::new(url).map_err(|e| WsError(format!("websocket {url}: {e:?}")))?;
        socket.set_binary_type(BinaryType::Arraybuffer);
        let lanes: Shared = Rc::default();

        let on_open = {
            let lanes = Rc::clone(&lanes);
            Closure::<dyn FnMut(JsValue)>::new(move |_| {
                lanes.borrow_mut().open = true;
                wake(&lanes);
            })
        };
        let on_message = {
            let lanes = Rc::clone(&lanes);
            Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                let Ok(buffer) = event.data().dyn_into::<js_sys::ArrayBuffer>() else {
                    return;
                };
                let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
                let Some((&lane, payload)) = bytes.split_first() else {
                    return;
                };
                {
                    let mut lanes = lanes.borrow_mut();
                    if let Some(count) = lanes.received.get_mut(usize::from(lane)) {
                        *count += 1;
                    }
                    match lane {
                        LANE_CONTROL => lanes.control.extend(payload),
                        LANE_DATAGRAM => {
                            if lanes.datagrams.len() >= MAX_QUEUED_DATAGRAMS {
                                lanes.datagrams.pop_front();
                                lanes.dropped_datagrams += 1;
                            }
                            lanes.datagrams.push_back(payload.to_vec());
                        }
                        LANE_UNI => lanes.unis.push_back(payload.to_vec()),
                        _ => return,
                    }
                }
                wake(&lanes);
            })
        };
        let on_close = {
            let lanes = Rc::clone(&lanes);
            Closure::<dyn FnMut(CloseEvent)>::new(move |event: CloseEvent| {
                let reason = event.reason();
                close_with(
                    &lanes,
                    if reason.is_empty() {
                        format!("websocket closed (code {})", event.code())
                    } else {
                        reason
                    },
                );
            })
        };
        let on_error = {
            let lanes = Rc::clone(&lanes);
            Closure::<dyn FnMut(JsValue)>::new(move |_| {
                close_with(&lanes, "websocket error".into());
            })
        };
        socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        socket.set_onerror(Some(on_error.as_ref().unchecked_ref()));

        let connection = Self(Rc::new(Inner {
            socket,
            lanes: Rc::clone(&lanes),
            rtt: Cell::new(Duration::from_millis(1)),
            _callbacks: (on_open, on_message, on_close, on_error),
        }));
        let started = Instant::now();
        wait(&lanes, |state| state.open.then_some(()))
            .await
            .map_err(|error| WsError(format!("{url}: {error}")))?;
        // The WebSocket handshake is one round trip over an open TCP stream.
        connection
            .0
            .rtt
            .set(started.elapsed().max(Duration::from_millis(1)));
        Ok(connection)
    }

    fn send(&self, lane: u8, bytes: &[u8]) -> Result<(), WsError> {
        if let Some(reason) = &self.0.lanes.borrow().closed {
            return Err(WsError(reason.clone()));
        }
        // A fresh, unshared copy: WebSocket.send refuses views of shared memory.
        let message = js_sys::Uint8Array::new_with_length(bytes.len() as u32 + 1);
        message.set_index(0, lane);
        message.subarray(1, bytes.len() as u32 + 1).copy_from(bytes);
        self.0
            .socket
            .send_with_array_buffer_view(&message)
            .map_err(|e| WsError(format!("websocket send: {e:?}")))?;
        self.0.lanes.borrow_mut().sent[usize::from(lane)] += 1;
        Ok(())
    }

    /// Messages sent and received per lane since the socket opened, and the
    /// bytes the browser has queued but not yet written to the network.
    pub fn traffic(&self) -> String {
        let lanes = self.0.lanes.borrow();
        format!(
            "sent ctrl={} dgram={} uni={} | recv ctrl={} dgram={} uni={} | dgram queued={} dropped={} | socket buffered={}B",
            lanes.sent[0],
            lanes.sent[1],
            lanes.sent[2],
            lanes.received[0],
            lanes.received[1],
            lanes.received[2],
            lanes.datagrams.len(),
            lanes.dropped_datagrams,
            self.0.socket.buffered_amount(),
        )
    }

    pub async fn open_bi(&self) -> Result<(CtrlSend, CtrlRecv), WsError> {
        Ok((CtrlSend(self.clone()), CtrlRecv(self.clone())))
    }

    pub async fn read_datagram(&self) -> Result<Vec<u8>, WsError> {
        wait(&self.0.lanes, |state| state.datagrams.pop_front()).await
    }

    pub fn send_datagram(&self, bytes: Vec<u8>) -> Result<(), WsError> {
        self.send(LANE_DATAGRAM, &bytes)
    }

    pub async fn accept_uni(&self) -> Result<UniRecv, WsError> {
        wait(&self.0.lanes, |state| state.unis.pop_front())
            .await
            .map(UniRecv)
    }

    pub async fn open_uni(&self) -> Result<UniSend, WsError> {
        Ok(UniSend {
            connection: self.clone(),
            bytes: Vec::new(),
        })
    }

    pub fn rtt(&self) -> Duration {
        self.0.rtt.get()
    }

    pub fn close_reason(&self) -> Option<String> {
        self.0.lanes.borrow().closed.clone()
    }

    pub fn close(&self, _code: u32, reason: &[u8]) {
        close_with(&self.0.lanes, String::from_utf8_lossy(reason).into_owned());
        let _ = self.0.socket.close();
    }
}

pub struct CtrlSend(WsConnection);

impl CtrlSend {
    pub fn set_priority(&mut self, _priority: i32) -> Result<(), WsError> {
        Ok(())
    }

    pub async fn write_all(&mut self, bytes: &[u8]) -> Result<(), WsError> {
        self.0.send(LANE_CONTROL, bytes)
    }
}

pub struct CtrlRecv(WsConnection);

impl CtrlRecv {
    pub async fn read_exact(&mut self, out: &mut [u8]) -> Result<(), WsError> {
        let want = out.len();
        wait(&self.0.0.lanes, |state| {
            (state.control.len() >= want).then(|| {
                for (slot, byte) in out.iter_mut().zip(state.control.drain(..want)) {
                    *slot = byte;
                }
            })
        })
        .await
    }
}

pub struct UniRecv(Vec<u8>);

impl UniRecv {
    pub async fn read_to_end(&mut self, limit: usize) -> Result<Vec<u8>, WsError> {
        if self.0.len() > limit {
            return Err(WsError(format!("uni stream {} bytes over {limit}", self.0.len())));
        }
        Ok(std::mem::take(&mut self.0))
    }
}

/// Collects one unidirectional stream; `finish` sends it as one message.
pub struct UniSend {
    connection: WsConnection,
    bytes: Vec<u8>,
}

impl UniSend {
    pub fn set_priority(&mut self, _priority: i32) -> Result<(), WsError> {
        Ok(())
    }

    pub async fn write_all(&mut self, bytes: &[u8]) -> Result<(), WsError> {
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    pub fn finish(&mut self) -> Result<(), WsError> {
        self.connection.send(LANE_UNI, &self.bytes)
    }
}
