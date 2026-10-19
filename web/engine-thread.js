// A thread of the engine (crates/web_threads/src/web.rs): the engine worker
// starts one with the module, the shared memory and a pointer to the closure
// to run. This scope gets the file system and the audio ring; the page's
// other bridges (screen, LAN status, decoding) stay with the engine worker,
// which runs calls into them for this thread (web_threads::on_engine).
//
// A resident thread (the game thread, web_threads::web::spawn_resident)
// stays after its closure: it waits on its step word without blocking and
// runs a step each time the word changes, so between steps its event loop
// runs the futures, timers and sockets the steps started.
// Panic reports carry the Rust call stack; the default 10 frames end inside
// the panic machinery.
Error.stackTraceLimit = 64;
import { installVfs, polyfillWaitAsync } from "./engine-shared.js";

polyfillWaitAsync();

/** Steps of a resident thread, until the engine is gone. */
async function resident(glue, memory, word) {
  const words = new Int32Array(memory.buffer);
  const at = word >> 2;
  let seen = 0;
  for (;;) {
    const wait = Atomics.waitAsync(words, at, seen);
    if (wait.async) await wait.value;
    seen = Atomics.load(words, at);
    glue.iw4l_thread_step();
  }
}

self.onmessage = async (event) => {
  const boot = event.data;
  self.onmessage = null;
  const relay = (message) => self.postMessage(message);
  const step = (text) => relay({ type: "log", text });
  // A panic on this thread: the page shows its message when the engine stops.
  const consoleError = console.error.bind(console);
  console.error = (...args) => {
    consoleError(...args);
    const panic = args.map(String).join(" ").match(/panicked at [^\n]*\n([^\n]+)/);
    if (panic) relay({ type: "panic", text: panic[1] });
  };
  step("boot received");
  // A thread may block, so a warm request is answered by reading at once.
  installVfs({ ...boot.vfs, label: boot.name }, relay, { warm: () => true });
  globalThis.iw4lAudioRing = boot.audio;
  const started = performance.now();
  try {
    const glue = await import(`./${boot.pkg}/iw4l.js`);
    step("glue imported");
    await glue.default({ module_or_path: boot.module, memory: boot.memory, thread_stack_size: boot.stack });
    relay({ type: "ready", ms: Math.round(performance.now() - started) });
    // A probe only proves this browser runs the module in a nested worker.
    if (!boot.probe) {
      glue.iw4l_thread_entry(boot.entry);
      const word = glue.iw4l_thread_resident?.() ?? 0;
      if (word) {
        step("resident");
        await resident(glue, boot.memory, word);
      }
    }
  } catch (error) {
    console.error(`thread ${boot.name} failed:`, error);
    relay({ type: "failed", message: String(error?.message || error) });
  }
  relay({ type: "done" });
  close();
};
