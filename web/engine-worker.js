// The engine's own thread (crates/bootstrap/src/web_worker.rs). The page
// sends one message to start: the compiled module, its canvas transferred
// as an OffscreenCanvas, the file reader's shared buffers and tables, and
// the audio ring it feeds its worklet from. After that the page relays input
// here and this worker relays the engine's page hooks (screen, LAN status,
// cursor lock, sound decoding, cache writes) back. The threaded engine
// spawns its worker threads from here too (crates/web_threads).
// Panic reports carry the Rust call stack; the default 10 frames end inside
// the panic machinery.
Error.stackTraceLimit = 64;
import { installVfs, polyfillWaitAsync } from "./engine-shared.js";

polyfillWaitAsync();

/** Streamed clips decoded by the page's audio decoder
 * (crates/audio/src/web_decode.rs): same poll protocol, answered by message. */
function installDecoder(page) {
  const jobs = new Map();
  let next = 0;
  globalThis.iw4lDecode = {
    submit(bytes) {
      const id = ++next;
      jobs.set(id, { state: 0, channels: 0, rate: 0, samples: null, error: "" });
      const copy = bytes.slice();
      page.postMessage({ type: "decode", id, bytes: copy }, [copy.buffer]);
      return id;
    },
    state: (id) => jobs.get(id)?.state ?? -1,
    channels: (id) => jobs.get(id)?.channels ?? 0,
    rate: (id) => jobs.get(id)?.rate ?? 0,
    take(id) {
      const job = jobs.get(id);
      jobs.delete(id);
      return job?.samples ?? new Float32Array(0);
    },
    error(id) {
      const job = jobs.get(id);
      jobs.delete(id);
      return job?.error ?? "unknown decode job";
    },
  };
  return {
    decoded(message) {
      const job = jobs.get(message.id);
      if (!job) return;
      if (message.error) {
        job.error = message.error;
        job.state = -1;
      } else {
        job.channels = message.channels;
        job.rate = message.rate;
        job.samples = message.samples;
        job.state = 1;
      }
    },
  };
}

/** Whether this browser runs the module in a nested worker over the shared
 * memory: one probe thread, given a few seconds. Firefox has failed this
 * silently; the engine then runs single-threaded in this worker. */
function probeThreads(boot, module, memory) {
  return new Promise((resolve) => {
    const started = performance.now();
    let worker;
    const finish = (ok, why) => {
      clearTimeout(timer);
      try { worker?.terminate(); } catch {}
      console.log(`threads: probe ${ok ? "ok" : "failed"} (${why}) in ${Math.round(performance.now() - started)} ms`);
      resolve(ok);
    };
    const timer = setTimeout(() => finish(false, "no answer in 4 s"), 4000);
    try {
      worker = new Worker(new URL("./engine-thread.js", import.meta.url), { type: "module", name: "probe" });
      worker.onerror = (event) => finish(false, event.message ?? "error");
      worker.onmessage = (event) => {
        const message = event.data;
        if (message?.type === "ready") finish(true, `ready in ${message.ms} ms`);
        else if (message?.type === "failed") finish(false, message.message);
        else if (message?.type === "log") console.log(`thread probe: ${message.text}`);
      };
      worker.postMessage({ pkg: boot.pkg, module, memory, stack: 1 << 20, entry: 0, name: "probe", probe: true, vfs: boot.vfs, audio: boot.audio });
    } catch (error) {
      finish(false, String(error?.message || error));
    }
  });
}

/** The engine's threads (crates/web_threads/src/web.rs): each a worker over
 * the same memory, given this worker's file system and audio ring. */
function installThreads(boot, page) {
  const threads = new Set();
  globalThis.iw4lThreads = {
    // `?cores=N` stands in for a smaller machine when testing.
    // `?cores=N` stands in for a smaller machine in tests (with the browser
    // pinned to N logical processors).
    concurrency: () => Number(new URLSearchParams(boot.search || "").get("cores")) || navigator.hardwareConcurrency || 4,
    spawn(name, stack, entry, module, memory) {
      const worker = new Worker(new URL("./engine-thread.js", import.meta.url), { type: "module", name });
      threads.add(worker);
      worker.onerror = (event) => console.error(`thread ${name}: ${event.message ?? event}`);
      worker.onmessage = (event) => {
        const message = event.data;
        switch (message?.type) {
          case "cacheWrite":
          case "vfs":
            page.postMessage(message);
            break;
          case "ready":
            console.log(`thread ${name}: ready in ${message.ms} ms`);
            break;
          case "log":
            console.log(`thread ${name}: ${message.text}`);
            break;
          case "failed":
            console.error(`thread ${name} failed: ${message.message}`);
            break;
          case "panic":
            page.postMessage(message);
            break;
          case "done":
            threads.delete(worker);
            worker.terminate();
            break;
        }
      };
      worker.postMessage({
        pkg: boot.pkg,
        module,
        memory,
        stack,
        entry,
        name,
        vfs: boot.vfs,
        audio: boot.audio,
      });
    },
  };
}

async function start(boot) {
  const page = self;
  const t0 = performance.now();
  const relay = (message) => page.postMessage(message);
  const { warmed } = installVfs({ ...boot.vfs, label: "engine" }, relay);
  const decoder = installDecoder(page);
  // The page made the worklet; the engine only fills its ring.
  globalThis.iw4lAudioRing = boot.audio;
  // The engine's page hooks (crates/net/src/transport/master.rs, plugin.rs).
  globalThis.iw4lScreen = (screen) => page.postMessage({ type: "screen", screen });
  globalThis.iw4lLanStatus = (text) => page.postMessage({ type: "lan", text });
  globalThis.iw4lLanEnded = (reason) => page.postMessage({ type: "ended", reason });
  globalThis.iw4lClassesDone = () => page.postMessage({ type: "classes" });
  globalThis.iw4lFrameStats = (fps, mean, p90) => page.postMessage({ type: "frame-stats", fps, mean, p90 });
  globalThis.iw4lWorkerHost = {
    cursor: (locked, visible) => page.postMessage({ type: "cursor", locked, visible }),
  };
  // The page shows the panic's message when the engine stops.
  const consoleError = console.error.bind(console);
  console.error = (...args) => {
    consoleError(...args);
    const panic = args.map(String).join(" ").match(/panicked at [^\n]*\n([^\n]+)/);
    if (panic) page.postMessage({ type: "panic", text: panic[1] });
  };

  const glue = await import(`./${boot.pkg}/iw4l.js`);
  const t1 = performance.now();
  const wasm = await glue.default(boot.module ? { module_or_path: boot.module } : undefined);
  globalThis.iw4lWasm = wasm;
  console.log(`engine worker: boot received, glue in ${(t1 - t0).toFixed(0)} ms, instantiated in ${(performance.now() - t1).toFixed(0)} ms`);
  // A shared-memory build may use threads; only install the spawner where a
  // probe thread actually comes up, so the engine never waits on threads
  // that will not run.
  if (wasm.memory?.buffer instanceof SharedArrayBuffer && await probeThreads(boot, boot.module, wasm.memory)) {
    installThreads(boot, page);
  }

  self.onmessage = (event) => {
    const message = event.data;
    switch (message.type) {
      case "input":
        glue.iw4l_worker_input(message.kind, message.a, message.b, message.c, message.s);
        break;
      case "decoded":
        decoder.decoded(message);
        break;
      case "warmed":
        warmed.set(message.id, true);
        break;
    }
  };
  glue.iw4l_worker_main(
    boot.canvas,
    boot.search,
    boot.protocol,
    boot.host,
    boot.width,
    boot.height,
    boot.scale,
  );
  page.postMessage({ type: "started" });
}

// The boot message carries the module; input the page sends before it (the
// resize observer fires at once) waits until the engine's handler is up.
const early = [];
let booting = false;
self.onmessage = (event) => {
  const message = event.data;
  if (booting || message?.type) {
    early.push(event);
    return;
  }
  booting = true;
  start(message)
    .then(() => {
      for (const kept of early) self.onmessage(kept);
      early.length = 0;
    })
    .catch((error) => {
      console.error("engine worker failed to start", error);
      self.postMessage({ type: "failed", message: String(error?.message || error) });
    });
};
