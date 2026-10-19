// Shared by the engine worker (web/engine-worker.js) and the threads it
// spawns (web/engine-thread.js): the file system the engine reads through,
// and a stand-in for Atomics.waitAsync where the browser lacks it.

const IDLE = 0;
const REQUEST = 1;
const DONE = 2;
const CLAIMED = 3;

/** Firefox before 140 has no Atomics.waitAsync, which the threaded engine's
 * task wake-ups use (js-sys's multithreaded futures). A helper worker that
 * blocks in Atomics.wait and answers by message stands in for it. */
export function polyfillWaitAsync() {
  if (typeof Atomics.waitAsync === "function") return;
  const script = "onmessage = (e) => { const [buffer, byteOffset, index, value, timeout] = e.data;" +
    " const words = new Int32Array(buffer, byteOffset); postMessage(Atomics.wait(words, index, value, timeout)); };";
  const url = URL.createObjectURL(new Blob([script], { type: "text/javascript" }));
  const idle = [];
  Atomics.waitAsync = (words, index, value, timeout = Infinity) => {
    if (Atomics.load(words, index) !== value) return { async: false, value: "not-equal" };
    if (timeout <= 0) return { async: false, value: "timed-out" };
    const helper = idle.pop() ?? new Worker(url);
    const answer = new Promise((resolve) => {
      helper.onmessage = (event) => {
        idle.push(helper);
        resolve(event.data);
      };
    });
    helper.postMessage([words.buffer, words.byteOffset, index, value, timeout]);
    return { async: true, value: answer };
  };
}

/**
 * The file system the engine reads through (crates/vfs/src/web.rs), over
 * the same shared buffers the page's vfs.js uses, but sleeping on the
 * request word instead of spinning: a worker may Atomics.wait. `vfs` is the
 * page's description (buffers, entries, cache snapshot); `relay` posts to
 * whoever can reach the page (cache writes, warm requests); `warm` replaces
 * the warm request where no reply can come back (threads read at once).
 */
export function installVfs(vfs, relay, { warm: warmOverride } = {}) {
  const control = new Int32Array(vfs.control);
  const offset = new Float64Array(vfs.offset);
  const data = new Uint8Array(vfs.data);
  // Slots as the page laid them out (see SLOTS in vfs.js): four control
  // words each, then the request and release counters.
  const SLOTS = (control.length - 2) >> 2;
  const SLOT_WINDOW = Math.floor(data.length / SLOTS);
  const DOORBELL = SLOTS * 4;
  const FREED = SLOTS * 4 + 1;
  const net = new Float64Array(vfs.stats);
  const entries = vfs.entries;
  const cache = new Map(vfs.cache);
  const cached = [...cache.keys()];
  const stats = { reads: 0, bytes: 0, ms: 0, since: performance.now() };
  const label = vfs.label || "worker";

  function claimSlot() {
    for (;;) {
      for (let s = 0; s < SLOTS; s++) {
        if (Atomics.compareExchange(control, s * 4, IDLE, CLAIMED) === IDLE) return s;
      }
      // Every slot taken: sleep until one is released, then look again.
      const seen = Atomics.load(control, FREED);
      Atomics.wait(control, FREED, seen, 20);
    }
  }

  function read(index, at, dst) {
    const started = performance.now();
    const slot = claimSlot();
    const base = slot * 4;
    const window = data.subarray(slot * SLOT_WINDOW, (slot + 1) * SLOT_WINDOW);
    let done = 0;
    while (done < dst.length) {
      const want = Math.min(SLOT_WINDOW, dst.length - done);
      control[base + 1] = index;
      control[base + 2] = want;
      offset[slot] = at + done;
      Atomics.store(control, base, REQUEST);
      Atomics.add(control, DOORBELL, 1);
      Atomics.notify(control, DOORBELL);
      for (;;) {
        const state = Atomics.load(control, base);
        if (state === DONE) break;
        Atomics.wait(control, base, state);
      }
      const got = control[base + 3];
      Atomics.store(control, base, CLAIMED);
      if (got <= 0) break;
      dst.set(window.subarray(0, got), done);
      done += got;
      if (got < want) break;
    }
    Atomics.store(control, base, IDLE);
    Atomics.add(control, FREED, 1);
    Atomics.notify(control, FREED);
    const now = performance.now();
    stats.reads += 1;
    stats.bytes += done;
    stats.ms += now - started;
    if (now - stats.since > 5000) {
      const hosted = vfs.hosted
        ? `; host ${(net[0] / 1048576).toFixed(1)} MiB in ${net[1]} requests, ` +
          `disk ${(net[3] / 1048576).toFixed(1)} MiB, memory hits ${net[2]}`
        : "";
      console.log(
        `vfs (${label}): ${stats.reads} reads, ${(stats.bytes / 1048576).toFixed(1)} MiB, ` +
          `${stats.ms.toFixed(0)} ms waiting of ${(now - stats.since).toFixed(0)} ms${hosted}`,
      );
      Object.assign(stats, { reads: 0, bytes: 0, ms: 0, since: now });
    }
    return done;
  }

  // Ranges the engine wants in memory before it reads them; the page's read
  // worker loads them and the page relays its "warmed" reply here.
  const warmed = new Map();
  function warm(index, at, length) {
    if (net[5] < 1) return true;
    const id = `${index}:${at}:${length}`;
    const state = warmed.get(id);
    if (state !== undefined) return state;
    warmed.set(id, false);
    relay({ type: "vfs", message: { type: "warm", id, entry: index, at, length } });
    return false;
  }

  globalThis.iw4lVfs = {
    mountRoot: () => vfs.mountRoot,
    warm: warmOverride ?? warm,
    cacheCount: () => cached.length,
    cachePath: (i) => cached[i],
    cacheSize: (i) => cache.get(cached[i])?.length ?? 0,
    cacheRead(path, at, dst) {
      const bytes = cache.get(path);
      if (!bytes) return 0;
      const part = bytes.subarray(at, at + dst.length);
      dst.set(part);
      return part.length;
    },
    cacheWrite(path, src) {
      // `src` views the engine's memory: copy it, once for us and once for
      // the page's persistent store.
      const bytes = src.slice();
      cache.set(path, bytes);
      relay({ type: "cacheWrite", path, bytes: bytes.slice() });
      return true;
    },
    entryCount: () => entries.length,
    entryPath: (i) => entries[i].path,
    entrySize: (i) => entries[i].size,
    entryModified: (i) => entries[i].modified,
    read,
  };
  return { warmed };
}
