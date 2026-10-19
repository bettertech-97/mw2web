// The page half of the engine's browser file system (crates/vfs/src/web.rs).
// `mount` publishes `globalThis.iw4lVfs` before the engine starts: an index of
// the player's games folder and a synchronous `read` the engine calls.

const IDLE = 0;
const REQUEST = 1;
const DONE = 2;
const CLAIMED = 3;
// Request slots on the shared channel: the threaded engine's load workers
// read at once, and one slot serialised them behind a single read in flight
// (a load worker spent 10 s of a 30 s load waiting for the slot). Each slot
// has four control words [state, entry, length, result], an offset and a
// window of the data buffer; two counters after the slots ring the worker
// (requests) and wake waiters for a free slot (releases).
const SLOTS = 8;
const SLOT_WINDOW = 2 * 1024 * 1024;
/** Above this the persistent cache is cleared rather than loaded whole. */
const CACHE_LIMIT = 256 * 1024 * 1024;

export const MOUNT_ROOT = "/games/Modern Warfare 2";

/** Set before mounting when the engine will run in its own worker: the read
 * worker may then fetch while the engine waits, in every browser. */
let engineInWorker = false;
export function setEngineInWorker(on) {
  engineInWorker = !!on;
}

/** The last mount's worker channel and tables, for an engine in a worker
 * (web/engine-worker.js) that reads through the same shared buffers. */
let mounted = null;
export function mountedChannel() {
  return mounted;
}

/** Starts the read worker over `source` ({files} or {urls}) and resolves once
 * it is listening. */
function startWorker(source) {
  const control = new SharedArrayBuffer((SLOTS * 4 + 2) * 4);
  const offset = new SharedArrayBuffer(8 * SLOTS);
  const data = new SharedArrayBuffer(SLOTS * SLOT_WINDOW);
  // Hosted-file traffic counters the worker keeps up to date; see the
  // NET_* indices in vfs-worker.js.
  const stats = new SharedArrayBuffer(8 * 8);
  const worker = new Worker(new URL("./vfs-worker.js", import.meta.url));
  return new Promise((resolve, reject) => {
    worker.onerror = reject;
    worker.onmessage = () =>
      resolve({
        worker,
        control: new Int32Array(control),
        offset: new Float64Array(offset),
        data: new Uint8Array(data),
        net: new Float64Array(stats),
      });
    worker.postMessage({ control, offset, data, source, stats, engineInWorker });
  });
}

/** What the worker's counters say, in the shape the page's progress line and
 * the periodic log read. */
function netSnapshot(net) {
  return {
    netBytes: net[0],
    netRequests: net[1],
    memoryHits: net[2],
    diskBytes: net[3],
    storedBytes: net[4],
    storeState: net[5],
  };
}

/**
 * The engine's small derived caches (translated shaders, bot navigation,
 * localization), kept in the origin-private file system between loads so a
 * second load skips that work. They are the engine's own output, never game
 * data. One pack file, read whole before the engine starts; new entries are
 * appended in batches in the background. Records are
 * [u32 path bytes][path][u32 data bytes][data], little-endian; a later record
 * for the same path wins. Paths are relative to /iw4l-artifacts/cache.
 */
const CACHE_PACK = "iw4l-cache.pack";
const textEncoder = new TextEncoder();

/** Reads pack records into `files`; returns the offset after the last whole record. */
function parsePack(bytes, files) {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const decoder = new TextDecoder();
  let at = 0;
  while (at + 8 <= bytes.length) {
    const pathLen = view.getUint32(at, true);
    const pathEnd = at + 4 + pathLen;
    if (pathEnd + 4 > bytes.length) break;
    const dataLen = view.getUint32(pathEnd, true);
    const dataEnd = pathEnd + 4 + dataLen;
    if (dataEnd > bytes.length) break;
    files.set(decoder.decode(bytes.subarray(at + 4, pathEnd)), bytes.subarray(pathEnd + 4, dataEnd));
    at = dataEnd;
  }
  return at;
}

/** Records ([pathBytes, dataBytes] pairs) as one pack. */
function packRecords(batch) {
  const total = batch.reduce((sum, [path, bytes]) => sum + 8 + path.length + bytes.length, 0);
  const out = new Uint8Array(total);
  const view = new DataView(out.buffer);
  let at = 0;
  for (const [path, bytes] of batch) {
    view.setUint32(at, path.length, true);
    out.set(path, at + 4);
    at += 4 + path.length;
    view.setUint32(at, bytes.length, true);
    out.set(bytes, at + 4);
    at += 4 + bytes.length;
  }
  return out;
}

/** Appends a pack to the persistent cache file. */
async function appendPack(cache, out) {
  const writable = await cache.handle.createWritable({ keepExistingData: true });
  await writable.write({ type: "write", position: cache.size, data: out });
  await writable.close();
  cache.size += out.length;
}

/** Translated shaders, bot navigation and localization that another
 * player's engine already produced, offered by the relay: a cold cache takes
 * them so its first load skips that work (seconds of shader translation).
 * Entries already here win; a player's own profile is never seeded. */
async function seedCache(cache) {
  try {
    let shaders = 0;
    for (const path of cache.files.keys()) if (path.startsWith("wgsl/") && ++shaders >= 200) return;
    const response = await fetch("/lan/cache-seed", { cache: "no-store" });
    if (!response.ok) return;
    const bytes = new Uint8Array(await response.arrayBuffer());
    const offered = new Map();
    parsePack(bytes, offered);
    const fresh = [];
    for (const [path, data] of offered) {
      if (path.startsWith("profile/") || cache.files.has(path)) continue;
      cache.files.set(path, data);
      fresh.push([textEncoder.encode(path), data]);
    }
    if (fresh.length && cache.handle) await appendPack(cache, packRecords(fresh));
    console.log(`vfs: cache seeded with ${fresh.length} of ${offered.size} entries (${(bytes.length / 1048576).toFixed(1)} MB)`);
  } catch (error) {
    console.warn("vfs: cache seed", error);
  }
}

/** This PC's translated shaders, navigation and localization as a gzip
 * pack for the relay to hand to the next player's cold cache; null until
 * there is enough to be worth it, or when it would not beat the relay's own
 * (`keptRecords`: the relay keeps the richest seed, and compressing tens of
 * MB costs seconds of the page's main thread mid-match). Read from the
 * persistent pack file, which is where an engine in a worker lands its
 * entries. */
export async function cacheSeed(keptRecords = 0) {
  const files = new Map();
  try {
    const root = await navigator.storage.getDirectory();
    const file = await (await root.getFileHandle(CACHE_PACK)).getFile();
    parsePack(new Uint8Array(await file.arrayBuffer()), files);
  } catch {
    return null;
  }
  const batch = [];
  for (const [path, bytes] of files) {
    if (/^(wgsl|nav|localize)\//.test(path)) batch.push([textEncoder.encode(path), bytes]);
  }
  if (batch.length < 100 || batch.length <= keptRecords) return null;
  const raw = packRecords(batch);
  const blob = await new Response(new Blob([raw]).stream().pipeThrough(new CompressionStream("gzip"))).blob();
  return { records: batch.length, blob, bytes: raw.length };
}

async function openCache() {
  const files = new Map();
  try {
    const root = await navigator.storage.getDirectory();
    // Earlier builds kept one file per entry, which loads ~100x slower.
    root.removeEntry("iw4l-cache", { recursive: true }).catch(() => {});
    let handle = await root.getFileHandle(CACHE_PACK, { create: true });
    const file = await handle.getFile();
    if (file.size > CACHE_LIMIT) {
      await root.removeEntry(CACHE_PACK);
      handle = await root.getFileHandle(CACHE_PACK, { create: true });
      return { handle, size: 0, files };
    }
    const bytes = new Uint8Array(await file.arrayBuffer());
    const at = parsePack(bytes, files);
    // A torn last record (tab closed mid-append) is dropped by appending after it.
    return { handle, size: at, files };
  } catch (error) {
    console.warn("vfs: persistent cache unavailable", error);
    return { handle: null, size: 0, files };
  }
}

/**
 * entries: [{path, size, modified}], paths relative to the game folder with
 * forward slashes. source: {files: File[]} parallel to entries, or {urls}.
 */
export async function mount(entries, source) {
  const [channel, cache] = await Promise.all([startWorker(source), openCache()]);
  if (source.urls) await seedCache(cache);
  const { control, offset, data, net } = channel;
  const cached = [...cache.files.keys()];
  const stats = { reads: 0, bytes: 0, ms: 0, since: performance.now() };
  globalThis.iw4lVfsStats = stats;
  globalThis.iw4lVfsNet = () => netSnapshot(net);
  // The page may show hosted-file progress; the main thread only runs between
  // engine reads, so report at most twice a second from inside read().
  let reported = 0;

  const DOORBELL = SLOTS * 4;
  const FREED = SLOTS * 4 + 1;
  function read(index, at, dst) {
    const started = performance.now();
    // The main thread may not Atomics.wait; spin for a slot (always free on
    // this single-threaded path) and for the worker's answer.
    let slot = -1;
    while (slot < 0) {
      for (let s = 0; s < SLOTS && slot < 0; s++) {
        if (Atomics.compareExchange(control, s * 4, IDLE, CLAIMED) === IDLE) slot = s;
      }
    }
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
      while (Atomics.load(control, base) !== DONE) {}
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
    const key = `${entries[index].path}`;
    stats.by = stats.by || {};
    const row = (stats.by[key] = stats.by[key] || { n: 0, b: 0, small: 0 });
    row.n += 1;
    row.b += done;
    if (dst.length < 65536) row.small += 1;
    stats.ms += now - started;
    if (source.urls && now - reported > 500) {
      reported = now;
      try {
        globalThis.iw4lVfsProgress?.(netSnapshot(net));
      } catch {}
    }
    if (now - stats.since > 5000) {
      const heap = globalThis.iw4lWasm?.memory?.buffer?.byteLength;
      const hosted = source.urls
        ? `; host ${(net[0] / 1048576).toFixed(1)} MiB in ${net[1]} requests, ` +
          `disk ${(net[3] / 1048576).toFixed(1)} MiB, memory hits ${net[2]}`
        : "";
      console.log(
        `vfs: ${stats.reads} reads, ${(stats.bytes / 1048576).toFixed(1)} MiB, ` +
          `${stats.ms.toFixed(0)} ms waiting of ${(now - stats.since).toFixed(0)} ms` +
          (heap ? `; wasm heap ${(heap / 1048576).toFixed(0)} MiB` : "") +
          hosted,
      );
      const top = Object.entries(stats.by || {})
        .sort((a, b) => b[1].b - a[1].b)
        .slice(0, 5)
        .map(([k, v]) => `${k} n=${v.n} small=${v.small} ${(v.b / 1048576).toFixed(1)}MiB`);
      console.log("vfs top: " + top.join(" | "));
      Object.assign(stats, { reads: 0, bytes: 0, ms: 0, since: now, by: {} });
    }
    return done;
  }

  function cacheRead(path, at, dst) {
    const bytes = cache.files.get(path);
    if (!bytes) return 0;
    const part = bytes.subarray(at, at + dst.length);
    dst.set(part);
    return part.length;
  }

  // `src` views the engine's memory: copy it before the background append.
  const encoder = new TextEncoder();
  let pending = [];
  let flushing = Promise.resolve();
  let timer = 0;
  function flush() {
    timer = 0;
    const batch = pending;
    pending = [];
    const out = packRecords(batch);
    flushing = flushing
      .then(() => appendPack(cache, out))
      .catch((error) => console.warn("vfs: cache save failed", error));
  }
  function cacheWrite(path, src) {
    const bytes = src.slice();
    cache.files.set(path, bytes);
    if (cache.handle) {
      pending.push([encoder.encode(path), bytes]);
      timer ||= setTimeout(flush, 2000);
    }
    // The player's profile (account, classes) also goes to the server's
    // account for this player, when the page has one.
    if (path.startsWith("profile/")) {
      try { globalThis.iw4lProfileWrite?.(path, bytes); } catch {}
    }
    return true;
  }

  /** Puts a file into the cache the engine will list at start (the
   * server's copy of a profile file, say); replaces any local copy. */
  function cacheSet(path, bytes) {
    cache.files.set(path, bytes);
    if (!cached.includes(path)) cached.push(path);
  }

  // Ranges the engine asked to have in memory before it reads them (sound
  // clips the first time they play): false while the worker loads them.
  const warmed = new Map();
  function warm(index, at, length) {
    // A worker in its blocking mode (no Atomics.waitAsync) never reads a
    // message; the range is read synchronously as before.
    if (net[5] < 1) return true;
    const k = `${index}:${at}:${length}`;
    const state = warmed.get(k);
    if (state !== undefined) return state;
    warmed.set(k, false);
    channel.worker.postMessage({ type: "warm", id: k, entry: index, at, length });
    return false;
  }
  channel.worker.addEventListener("message", (event) => {
    if (event.data?.type === "warmed") warmed.set(event.data.id, true);
  });
  globalThis.iw4lVfs = {
    mountRoot: () => MOUNT_ROOT,
    warm,
    cacheCount: () => cached.length,
    cachePath: (i) => cached[i],
    cacheSize: (i) => cache.files.get(cached[i])?.length ?? 0,
    cacheRead,
    cacheWrite,
    entryCount: () => entries.length,
    entryPath: (i) => entries[i].path,
    entrySize: (i) => entries[i].size,
    entryModified: (i) => entries[i].modified,
    read,
  };
  channel.entries = entries;
  channel.cache = cache;
  channel.cacheSet = cacheSet;
  // Writes now rather than after the usual pause (before leaving the page).
  channel.cacheFlush = () => {
    if (timer) {
      clearTimeout(timer);
      flush();
    }
    return flushing;
  };
  channel.source = source;
  mounted = channel;
  return channel;
}

/** The host's /games/ mount: the developer's own install on a dev server, or
 * the host PC's files for LAN and internet players. The worker fetches them
 * in blocks and keeps them; size and modified time key that cache. */
export async function mountDevServer() {
  const response = await fetch("/games-index.json");
  if (!response.ok) throw new Error("dev server has no --games folder");
  const entries = await response.json();
  // Bytes may live elsewhere (an object store the server names in `games`),
  // under the same relative paths; the index always comes from the server.
  const info = await fetch("/lan/info", { cache: "no-store" })
    .then((reply) => (reply.ok ? reply.json() : null))
    .catch(() => null);
  const encoded = entries.map((entry) => entry.path.split("/").map(encodeURIComponent).join("/"));
  const local = encoded.map((path) => `/games/${path}`);
  const base = info?.games ? info.games.replace(/\/+$/, "") : null;
  // The store holds the multiplayer set (archives, common and map zones);
  // anything else (the engine probes every zone's header at startup) comes
  // from the server, without a round trip to find that out.
  const stored = /^(main\/[^/]+\.iwd|zone\/english\/([^/]+_mp|mp_[^/]+)\.ff)$/i;
  const urls = base
    ? entries.map((entry, i) => (stored.test(entry.path) ? `${base}/${encoded[i]}` : local[i]))
    : local;
  hosted = await mount(entries, {
    urls,
    fallback: base ? local : null,
    paths: entries.map((entry) => entry.path),
    sizes: entries.map((entry) => entry.size),
    modified: entries.map((entry) => entry.modified),
  });
}

/** The hosted mount's worker channel, once mountDevServer has run. */
let hosted = null;

/** One reply of `type` from the hosted worker. */
function reply(type) {
  return new Promise((resolve) => {
    const onMessage = (event) => {
      if (event.data?.type !== type) return;
      hosted.worker.removeEventListener("message", onMessage);
      resolve(event.data);
    };
    hosted.worker.addEventListener("message", onMessage);
  });
}

/** The blocks this session has read from the host: {path: [[first, last]]}. */
export async function hostedManifest() {
  if (!hosted) return null;
  const answer = reply("manifest");
  hosted.worker.postMessage({ type: "manifest" });
  return (await answer).files;
}

/** Fetches a manifest's blocks into this PC's store before the engine needs
 * them, reporting {done, total} bytes to `onProgress`. Resolves to the bytes
 * fetched; 0 when nothing could be kept (no persistent store). */
export async function prefetchHosted(files, onProgress, parallel, pack) {
  if (!hosted) return 0;
  const { worker, net } = hosted;
  // The worker says whether it keeps blocks shortly after it starts; a
  // worker that cannot would never answer, so do not ask it.
  for (let waited = 0; net[5] === 0 && waited < 3000; waited += 50) {
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  if (net[5] < 1) return { bytes: 0, missing: 0 };
  const answer = reply("prefetched");
  worker.postMessage({ type: "prefetch", files, parallel, pack });
  const ticker = setInterval(() => {
    try {
      onProgress?.({ done: net[6], total: net[7] });
    } catch {}
  }, 250);
  try {
    const { bytes, missing } = await answer;
    return { bytes, missing: missing || 0 };
  } finally {
    clearInterval(ticker);
    try {
      onProgress?.({ done: net[6], total: net[7] });
    } catch {}
  }
}

async function walkHandle(dir, prefix, out) {
  for await (const [name, handle] of dir.entries()) {
    const path = prefix ? `${prefix}/${name}` : name;
    if (handle.kind === "directory") {
      await walkHandle(handle, path, out);
    } else {
      out.push({ path, file: await handle.getFile() });
    }
  }
}

/** Lists a picked folder: a FileSystemDirectoryHandle, or the FileList of an
 * <input webkitdirectory> (whose paths start with the folder's own name). */
export async function listPicked(picked) {
  const out = [];
  if (picked instanceof FileList || Array.isArray(picked)) {
    for (const file of picked) {
      const path = file.webkitRelativePath.split("/").slice(1).join("/");
      out.push({ path, file });
    }
  } else {
    await walkHandle(picked, "", out);
  }
  return out;
}

/** Mounts files from the player's folder; nothing is uploaded. */
export async function mountPicked(listed) {
  const entries = listed.map(({ path, file }) => ({
    path,
    size: file.size,
    modified: file.lastModified,
  }));
  await mount(entries, { files: listed.map(({ file }) => file) });
}

/** The MW2 multiplayer files a match needs; their absence means the wrong
 * folder was picked. */
export function missingGameFiles(paths) {
  const have = new Set(paths.map((path) => path.toLowerCase()));
  return ["zone/english/common_mp.ff", "zone/english/code_post_gfx_mp.ff", "main/iw_00.iwd"].filter(
    (path) => !have.has(path),
  );
}
