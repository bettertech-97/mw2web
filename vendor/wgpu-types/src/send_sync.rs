// iw4l: the Send/Sync shims stay on with wasm atomics; GPU objects are only
// ever touched from the page thread (worker threads load assets, not draw).
pub trait WasmNotSendSync: WasmNotSend + WasmNotSync {}
impl<T: WasmNotSend + WasmNotSync> WasmNotSendSync for T {}
#[cfg(any(
    not(target_arch = "wasm32"),
    feature = "fragile-send-sync-non-atomic-wasm"
))]
pub trait WasmNotSend: Send {}
#[cfg(any(
    not(target_arch = "wasm32"),
    feature = "fragile-send-sync-non-atomic-wasm"
))]
impl<T: Send> WasmNotSend for T {}
#[cfg(not(any(
    not(target_arch = "wasm32"),
    feature = "fragile-send-sync-non-atomic-wasm"
)))]
pub trait WasmNotSend {}
#[cfg(not(any(
    not(target_arch = "wasm32"),
    feature = "fragile-send-sync-non-atomic-wasm"
)))]
impl<T> WasmNotSend for T {}

#[cfg(any(
    not(target_arch = "wasm32"),
    feature = "fragile-send-sync-non-atomic-wasm"
))]
pub trait WasmNotSync: Sync {}
#[cfg(any(
    not(target_arch = "wasm32"),
    feature = "fragile-send-sync-non-atomic-wasm"
))]
impl<T: Sync> WasmNotSync for T {}
#[cfg(not(any(
    not(target_arch = "wasm32"),
    feature = "fragile-send-sync-non-atomic-wasm"
)))]
pub trait WasmNotSync {}
#[cfg(not(any(
    not(target_arch = "wasm32"),
    feature = "fragile-send-sync-non-atomic-wasm"
)))]
impl<T> WasmNotSync for T {}
