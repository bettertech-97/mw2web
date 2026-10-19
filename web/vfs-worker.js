// Answers the engine's file reads. The main thread cannot block, so it writes a
// request into shared memory and spins; this worker can block, reads the bytes
// synchronously, and flips the state word back.
//
// Sources: `files` — File objects from the folder the player picked (read with
// FileReaderSync, nothing leaves the machine); `urls` — the host's /games/
// mount, on the LAN or through its internet tunnel.
//
// Hosted files are fetched in aligned blocks and kept. The engine reads 16 KiB
// after every seek and doubles from there (crates/vfs/src/web.rs), so a zip
// archive produces floods of small reads; through a tunnel each one costs a
// ~0.2 s round trip, which a block cache turns into one per BLOCK. Blocks are
// also written to the origin-private file system, so a map a player has seen
// once never crosses the network again.

const IDLE = 0;
const REQUEST = 1;
const DONE = 2;
const CLAIMED = 3;
const SERVING = 4;

/** Small, because most hosted reads are small and scattered: a zone header
 * probe, or a texture's few-KiB prefix deep inside a 400 MB archive. A block
 * is the smallest unit fetched, so its size bounds how many bytes a tiny
 * read costs; a round trip costs the same whatever the size. Long sequential
 * reads are unaffected: their missing blocks merge into CHUNK requests. */
const BLOCK = 16 * 1024;
/** Part of every store name; bump it when the block layout changes so an
 * older store is left alone rather than misread. 3 was 64 KiB blocks: a map
 * touches ~2,300 textures at ~12 KB each, and every one cost a whole block,
 * 140 MB of a 360 MB first load. */
const LAYOUT = 4;
/** Bytes of blocks kept in memory; the least recently used go first. Half
 * on a PC that reports 4 GB or less: the engine's own heap is the big
 * tenant, and a tab that runs out of memory is killed, not slowed. */
const MEMORY_BYTES = ((navigator.deviceMemory || 8) <= 4 ? 128 : 256) * 1024 * 1024;
/** Fetches in flight for one read. A tunnel throttles each connection on its
 * own (measured: ~1 MB/s apiece, scaling with their number), so a long run
 * of missing blocks is split into requests of at most CHUNK bytes and fetched
 * together rather than as one slow stream. */
const PARALLEL = 6;
// A map's manifest is a thousand small ranges (textures inside the IWDs) at
// ~110 ms of latency each: the prefetch is bound by round trips, not by
// bandwidth, so it keeps many in flight. HTTP/2 multiplexes them on one
// connection.
const PREFETCH_PARALLEL = 12;
// Blocks between two wanted runs closer than this are fetched too: one
// request instead of two, for a few KiB nobody asked for.
const PREFETCH_GAP_BLOCKS = 4;
const CHUNK = 2 * 1024 * 1024;
/** Blocks fetched beyond what a read asked for, on files the engine reads
 * front to back: a zone's zlib stream and a Source map's own files. Without
 * this a map nobody has played on the server yet (no manifest to prefetch)
 * costs one round trip per small read, minutes for a 50 MB zone; with it,
 * one per MiB. Archives are read at scattered offsets and get none. */
const READ_AHEAD_BLOCKS = 64;

function readAheadEnd(entry, last) {
  const path = source.paths[entry] || "";
  // The sound pack is read one sound at a time and the texture packs one
  // image prefix at a time, like any archive: a MiB after each of those
  // read Inferno's 198 MB pack nearly whole for the 45 MB its tier wanted.
  if (!/(\.ff$|^iw4l-maps\/(?!.*\.(sounds|iwd)$))/i.test(path)) return last;
  return Math.min(last + READ_AHEAD_BLOCKS, blockCount(entry) - 1);
}
/** A tunnel hiccup must not end the match: each fetch retries with backoff. */
const RETRIES = 3;
const STORE_DIR = "iw4l-games";

// stats (Float64Array shared with the page): what crossed the network and
// what the caches answered, for the page's progress line and the 5 s log.
const NET_BYTES = 0;
const NET_REQUESTS = 1;
const MEMORY_HITS = 2;
const DISK_BYTES = 3;
const STORED_BYTES = 4;
/** 0 unknown, 1 keeping blocks on disk, -1 memory only. */
const STORE_STATE = 5;
/** Bytes fetched and bytes wanted by the prefetch under way. */
const PREFETCH_DONE = 6;
const PREFETCH_TOTAL = 7;

let control; // Int32Array: per slot [state, entry, length, result], then [requests, releases]
let offset; // Float64Array: byte offset per slot
let data; // Uint8Array: a transfer window per slot
let source;
let stats;
let SLOTS = 1;
let SLOT_WINDOW = 0;
let DOORBELL = 4;

function slotWindow(slot) {
  return data.subarray(slot * SLOT_WINDOW, (slot + 1) * SLOT_WINDOW);
}

function readFile(entry, at, length, window) {
  const file = source.files[entry];
  const end = Math.min(file.size, at + length);
  if (at >= end) return 0;
  const bytes = new FileReaderSync().readAsArrayBuffer(file.slice(at, end));
  window.set(new Uint8Array(bytes), 0);
  return bytes.byteLength;
}

// ---- block cache -----------------------------------------------------------

const memory = new Map(); // "entry:block" -> Uint8Array

function key(entry, block) {
  return `${entry}:${block}`;
}

// Block i covers file bytes [i * BLOCK, (i + 1) * BLOCK); the store mirrors
// that layout, so a block's offset on disk is its offset in the file.
function blockOf(at) {
  return Math.floor(at / BLOCK);
}

function blockStart(block) {
  return block * BLOCK;
}

function blockSize() {
  return BLOCK;
}

function blockLength(entry, block) {
  return Math.min(BLOCK, source.sizes[entry] - blockStart(block));
}

function blockCount(entry) {
  return Math.ceil(source.sizes[entry] / BLOCK);
}

function memoryGet(k) {
  const block = memory.get(k);
  if (block) {
    // Re-insert so Map order is recency.
    memory.delete(k);
    memory.set(k, block);
  }
  return block;
}

let memoryBytes = 0;

function memoryPut(k, block) {
  const old = memory.get(k);
  if (old) memoryBytes -= old.length;
  memory.set(k, block);
  memoryBytes += block.length;
  while (memoryBytes > MEMORY_BYTES && memory.size > 1) {
    const oldest = memory.keys().next().value;
    memoryBytes -= memory.get(oldest).length;
    memory.delete(oldest);
  }
}

/** Every block a read has touched, per entry: the list a player who has
 * loaded a map hands the host, so the next player can fetch it all before
 * their engine starts (see `manifest` and `prefetch`). */
const touched = new Map(); // entry -> Set(block)

function touch(entry, block) {
  let set = touched.get(entry);
  if (!set) touched.set(entry, (set = new Set()));
  set.add(block);
}

/** Consecutive missing block indices become [first, last] range requests of
 * at most CHUNK bytes each. */
function toRuns(missing) {
  const runs = [];
  let bytes = 0;
  for (const block of missing) {
    const last = runs[runs.length - 1];
    const size = blockSize(block);
    if (last && last[1] === block - 1 && bytes + size <= CHUNK) {
      last[1] = block;
      bytes += size;
    } else {
      runs.push([block, block]);
      bytes = size;
    }
  }
  return runs;
}

function runBounds(entry, first, last) {
  const start = blockStart(first);
  const end = Math.min(source.sizes[entry], blockStart(last) + blockSize(last));
  return [start, end];
}

/** The bytes of one range response, or a throw naming what went wrong. A
 * proxy that ignored Range answers 200 with the whole file; slice it rather
 * than fail, though that should not happen (the tunnel is verified to pass
 * Range through). */
function rangeBody(status, body, start, end, size) {
  let bytes = new Uint8Array(body);
  if (status === 200 && bytes.length === size) {
    bytes = bytes.subarray(start, end);
  } else if (status !== 206) {
    throw new Error(`range ${start}-${end - 1}: HTTP ${status}`);
  }
  if (bytes.length !== end - start) {
    throw new Error(`range ${start}-${end - 1}: got ${bytes.length} bytes`);
  }
  return bytes;
}

/** Splits a fetched run into blocks that own their memory, so the run buffer
 * can be dropped. Yields [block, bytes]. */
function* splitRun(bytes, first) {
  for (let block = first, at = 0; at < bytes.length; block++) {
    const n = blockSize(block);
    yield [block, bytes.slice(at, at + n)];
    at += n;
  }
}

/** Copies the requested span out of blocks now in memory. */
function copyOut(entry, at, end, window) {
  let done = 0;
  for (let block = blockOf(at); at + done < end; block++) {
    const bytes = memoryGet(key(entry, block));
    if (!bytes) break;
    const from = at + done - blockStart(block);
    const n = Math.min(bytes.length - from, end - at - done);
    if (n <= 0) break;
    window.set(bytes.subarray(from, from + n), done);
    done += n;
  }
  return done;
}

function backoff(attempt) {
  return 500 * 2 ** attempt;
}

// ---- synchronous hosted reads (no Atomics.waitAsync: memory cache only) ----

function fetchRunSync(entry, first, last) {
  const [start, end] = runBounds(entry, first, last);
  let error;
  for (let attempt = 0; attempt < RETRIES; attempt++) {
    try {
      const xhr = new XMLHttpRequest();
      xhr.open("GET", source.urls[entry], false);
      xhr.responseType = "arraybuffer";
      xhr.setRequestHeader("Range", `bytes=${start}-${end - 1}`);
      // Allowed on a synchronous request in a worker; a stalled connection
      // must throw and retry rather than hold the engine's read forever.
      xhr.timeout = fetchTimeoutMs(end - start);
      xhr.send();
      if (fallBack(entry, xhr.status)) {
        attempt -= 1;
        continue;
      }
      const bytes = rangeBody(xhr.status, xhr.response, start, end, source.sizes[entry]);
      stats[NET_REQUESTS] += 1;
      stats[NET_BYTES] += bytes.length;
      return bytes;
    } catch (e) {
      error = e;
      // Synchronous workers cannot sleep; a spin is the only backoff available.
      const until = Date.now() + backoff(attempt);
      while (Date.now() < until) {}
    }
  }
  throw error;
}

/** Set on a request's entry word by a thread whose reads should not join the
 * download list (vfs/web.rs UNTRACKED): background cache warming. */
const UNTRACKED = 0x40000000;

function readUrlSync(entry, at, length, window, track = true) {
  const size = source.sizes[entry];
  const end = Math.min(size, at + length);
  if (at >= end) return 0;
  const first = blockOf(at);
  const wanted = blockOf(end - 1);
  const last = readAheadEnd(entry, wanted);
  const missing = [];
  for (let block = first; block <= last; block++) {
    if (track && block <= wanted) touch(entry, block);
    if (memoryGet(key(entry, block))) {
      if (block <= wanted) stats[MEMORY_HITS] += 1;
    } else missing.push(block);
  }
  for (const [f, l] of toRuns(missing)) {
    for (const [block, bytes] of splitRun(fetchRunSync(entry, f, l), f)) {
      memoryPut(key(entry, block), bytes);
    }
  }
  return copyOut(entry, at, end, window);
}

/** The slots whose engine thread has posted a request, marked as taken. */
function pendingSlots() {
  const slots = [];
  for (let s = 0; s < SLOTS; s++) {
    if (Atomics.load(control, s * 4) === REQUEST) {
      Atomics.store(control, s * 4, SERVING);
      slots.push(s);
    }
  }
  return slots;
}

function answer(slot, result) {
  Atomics.store(control, slot * 4 + 3, result);
  Atomics.store(control, slot * 4, DONE);
  // An engine in a worker sleeps on its slot word instead of spinning.
  Atomics.notify(control, slot * 4);
}

function serveSync(read) {
  for (;;) {
    const seen = Atomics.load(control, DOORBELL);
    const slots = pendingSlots();
    for (const slot of slots) {
      let result;
      try {
        const raw = control[slot * 4 + 1];
        result = read(raw & ~UNTRACKED, offset[slot], control[slot * 4 + 2], slotWindow(slot), (raw & UNTRACKED) === 0);
      } catch (error) {
        console.error("vfs read failed", error);
        result = -1;
      }
      answer(slot, result);
    }
    // A request posted after `seen` was read returns this wait at once.
    if (slots.length === 0) Atomics.wait(control, DOORBELL, seen);
  }
}

// ---- persistent store --------------------------------------------------------

// One pair of files per hosted entry, named by a hash of path, size and
// modified time so a changed file never serves stale blocks:
//   <name>.blocks  block i at its own offset in the file (sparse)
//   <name>.map     one byte per block, 1 once that block's bytes are flushed
// Sync access handles let the read loop answer from disk without a round trip
// to the page; they exist only in dedicated workers, which this is.
const store = { root: null, enabled: false, entries: new Map() };

function hash(text) {
  let h = 5381;
  for (let i = 0; i < text.length; i++) h = ((h * 33) ^ text.charCodeAt(i)) >>> 0;
  return h.toString(16).padStart(8, "0");
}

function storeName(entry) {
  const path = source.paths[entry];
  const tail = path.slice(path.lastIndexOf("/") + 1).replace(/[^\w.-]/g, "_").slice(0, 60);
  return `${hash(`${LAYOUT}|${path}|${source.sizes[entry]}|${source.modified[entry]}`)}-${tail}`;
}

async function storeOpen() {
  try {
    const root = await navigator.storage.getDirectory();
    store.root = await root.getDirectoryHandle(STORE_DIR, { create: true });
    store.enabled = true;
    stats[STORE_STATE] = 1;
  } catch (error) {
    console.warn("vfs: persistent game cache unavailable", error);
    stats[STORE_STATE] = -1;
  }
}

function storeDisable(error) {
  console.warn("vfs: persistent game cache stopped", error);
  store.enabled = false;
  stats[STORE_STATE] = -1;
}

/** The entry's handles, opened on first use; null when the store is off. */
async function storeEntry(entry) {
  const known = store.entries.get(entry);
  if (known !== undefined) return known;
  // Reads of one file arrive together from several slots: the first opens
  // the handles, the rest await that same opening.
  const opening = openStoreEntry(entry);
  store.entries.set(entry, opening);
  const s = await opening;
  store.entries.set(entry, s);
  return s;
}

async function openStoreEntry(entry) {
  let s = null;
  if (store.enabled) {
    try {
      const name = storeName(entry);
      const blocks = await store.root.getFileHandle(`${name}.blocks`, { create: true });
      const map = await store.root.getFileHandle(`${name}.map`, { create: true });
      const present = new Uint8Array(blockCount(entry));
      s = {
        data: await blocks.createSyncAccessHandle(),
        map: await map.createSyncAccessHandle(),
        present,
        /** Blocks written since the last commit. */
        pending: [],
      };
      // A new map file is empty, so every block reads as absent.
      s.map.read(present, { at: 0 });
    } catch (error) {
      storeDisable(error);
      s = null;
    }
  }
  return s;
}

/** Blocks `first..last` (all present) in one disk read, into memory. */
function storeReadRun(s, entry, first, last) {
  const start = blockStart(first);
  const end = blockStart(last) + blockLength(entry, last);
  const bytes = new Uint8Array(end - start);
  const n = s.data.read(bytes, { at: start });
  if (n !== bytes.length) return false;
  stats[DISK_BYTES] += n;
  for (let block = first; block <= last; block++) {
    const at = blockStart(block) - start;
    memoryPut(key(entry, block), bytes.slice(at, at + blockLength(entry, block)));
  }
  return true;
}

function storeRead(s, entry, block) {
  const bytes = new Uint8Array(blockLength(entry, block));
  const n = s.data.read(bytes, { at: blockStart(block) });
  if (n !== bytes.length) return null;
  stats[DISK_BYTES] += n;
  return bytes;
}

/** Lands a block's bytes; `storeCommit` makes them count. Writes are cheap,
 * flushes are not, so a fetch batch commits once. */
function storeWrite(s, block, bytes) {
  if (!store.enabled) return;
  try {
    s.data.write(bytes, { at: blockStart(block) });
    s.pending.push(block);
    stats[STORED_BYTES] += bytes.length;
  } catch (error) {
    // Out of quota, most likely: play on from memory.
    storeDisable(error);
  }
}

/** Data is flushed before any map byte says it is there, so a present bit
 * never points at a torn block. */
function storeCommit(s) {
  if (!store.enabled || s.pending.length === 0) return;
  try {
    s.data.flush();
    for (const block of s.pending) s.map.write(new Uint8Array([1]), { at: block });
    s.map.flush();
    for (const block of s.pending) s.present[block] = 1;
  } catch (error) {
    storeDisable(error);
  }
  s.pending = [];
}

// ---- asynchronous hosted reads (memory, then disk, then the network) --------

/** A file the store does not hold (the engine probes every zone's header at
 * startup) is fetched from the server instead, from then on. */
function fallBack(entry, status) {
  if ((status === 404 || status === 403) && source.fallback && source.urls[entry] !== source.fallback[entry]) {
    source.urls[entry] = source.fallback[entry];
    return true;
  }
  return false;
}

/** How long one request may take before it is abandoned and retried: a
 * stalled connection otherwise holds a prefetch (and the page) forever. */
function fetchTimeoutMs(bytes) {
  // 30 s plus a 64 KB/s floor: a 2 MiB chunk gets about a minute, so a slow
  // home link finishes rather than times out and leaves the map incomplete.
  return 30000 + Math.ceil(bytes / (64 * 1024)) * 1000;
}

async function fetchRunAsync(entry, first, last) {
  const [start, end] = runBounds(entry, first, last);
  let error;
  for (let attempt = 0; attempt < RETRIES; attempt++) {
    const abort = new AbortController();
    const timer = setTimeout(() => abort.abort(), fetchTimeoutMs(end - start));
    try {
      const response = await fetch(source.urls[entry], {
        headers: { Range: `bytes=${start}-${end - 1}` },
        // The block store is the cache; the HTTP cache would only double it.
        cache: "no-store",
        signal: abort.signal,
      });
      if (fallBack(entry, response.status)) {
        clearTimeout(timer);
        attempt -= 1;
        continue;
      }
      const body = await response.arrayBuffer();
      clearTimeout(timer);
      const bytes = rangeBody(response.status, body, start, end, source.sizes[entry]);
      stats[NET_REQUESTS] += 1;
      stats[NET_BYTES] += bytes.length;
      return bytes;
    } catch (e) {
      clearTimeout(timer);
      error = e;
      await new Promise((resolve) => setTimeout(resolve, backoff(attempt)));
    }
  }
  throw error;
}

async function readUrlAsync(entry, at, length, window, track = true) {
  const size = source.sizes[entry];
  const end = Math.min(size, at + length);
  if (at >= end) return 0;
  await ensureBlocks(entry, at, end, track);
  return copyOut(entry, at, end, window);
}

/** Every block of [at, end) in memory: from the store, else fetched (and
 * stored). The read path and the engine's warm requests share it. */
async function ensureBlocks(entry, at, end, track = true) {
  const first = blockOf(at);
  const wanted = blockOf(end - 1);
  const last = readAheadEnd(entry, wanted);
  const s = await storeEntry(entry);
  const missing = [];
  // Blocks the store holds are read in runs, one disk call per run: a zone
  // is read whole by the engine, and one call per 16 KiB block was tens of
  // seconds of disk latency on some browsers.
  const onDisk = [];
  for (let block = first; block <= last; block++) {
    const k = key(entry, block);
    if (track && block <= wanted) touch(entry, block);
    if (memoryGet(k)) {
      if (block <= wanted) stats[MEMORY_HITS] += 1;
      continue;
    }
    if (s && s.present[block]) {
      // Read-ahead blocks already on disk are not worth a disk call now.
      if (block <= wanted) onDisk.push(block);
      continue;
    }
    missing.push(block);
  }
  for (const [f, l] of toRuns(onDisk)) {
    if (!storeReadRun(s, entry, f, l)) {
      for (let block = f; block <= l; block++) missing.push(block);
    }
  }
  missing.sort((a, b) => a - b);
  if (missing.length && serving > 0 && !NETWORK_WHILE_SERVING) {
    throw new Error(
      `${source.paths[entry]} blocks ${missing[0]}.. not on disk; this browser cannot fetch while the engine waits (prefetch first)`,
    );
  }
  const runs = toRuns(missing);
  for (let i = 0; i < runs.length; i += PARALLEL) {
    const batch = runs.slice(i, i + PARALLEL);
    const bodies = await Promise.all(batch.map(([f, l]) => fetchRunAsync(entry, f, l)));
    batch.forEach(([f], j) => {
      for (const [block, bytes] of splitRun(bodies[j], f)) {
        memoryPut(key(entry, block), bytes);
        if (s) storeWrite(s, block, bytes);
      }
    });
    if (s) storeCommit(s);
  }
}

async function warm(entry, at, length) {
  if (source.files) return;
  const size = source.sizes[entry];
  const end = Math.min(size, at + length);
  if (at >= end) return;
  await ensureBlocks(entry, at, end);
}

// Firefox has no Atomics.waitAsync; the loop yields through a message port
// instead (a setTimeout would be clamped to 4 ms) and polls the request word.
const yieldChannel = new MessageChannel();
function yieldToEvents() {
  return new Promise((resolve) => {
    yieldChannel.port1.onmessage = () => resolve();
    yieldChannel.port2.postMessage(0);
  });
}

// Firefox proxies a worker's network requests through the page's own thread,
// which is spinning on the request word while the engine waits: a fetch made
// to answer that request can never complete. Reads there must come from the
// store, which the prefetch fills before the engine starts.
// With the engine in its own worker the page thread is free, so the fetch
// completes whichever browser this is; the page says so when it starts us.
let NETWORK_WHILE_SERVING = typeof Atomics.waitAsync === "function";
let serving = 0;

/** One slot's request, answered when its blocks are in; several run at once,
 * so a fetch for one thread no longer holds up a disk read for another. */
async function serveSlot(slot) {
  let result;
  try {
    serving += 1;
    const raw = control[slot * 4 + 1];
    result = await readUrlAsync(raw & ~UNTRACKED, offset[slot], control[slot * 4 + 2], slotWindow(slot), (raw & UNTRACKED) === 0);
  } catch (error) {
    console.error(`vfs read failed: ${error && error.message ? error.message : error}`);
    result = -1;
  } finally {
    serving -= 1;
  }
  answer(slot, result);
}

async function serveAsync() {
  let idle = 0;
  for (;;) {
    const seen = Atomics.load(control, DOORBELL);
    const slots = pendingSlots();
    for (const slot of slots) serveSlot(slot);
    if (slots.length) {
      idle = 0;
      // Let the launched reads make progress before looking for more.
      await yieldToEvents();
      continue;
    }
    // A request posted after `seen` was read returns the wait at once.
    if (typeof Atomics.waitAsync === "function") {
      const waited = Atomics.waitAsync(control, DOORBELL, seen);
      if (waited.async) await waited.value;
    } else if (idle++ < 2000) {
      // Reads come in bursts: answer the next one within microseconds...
      await yieldToEvents();
    } else {
      // ...and once a burst is over, stop burning a core.
      await new Promise((resolve) => setTimeout(resolve, 2));
    }
  }
}

// ---- manifest and prefetch ---------------------------------------------------

/** What this session has read so far, as {path: [[first, last], ...]} block
 * runs; small enough to post to the host after every map. */
function manifest() {
  const files = {};
  for (const [entry, set] of touched) {
    const blocks = [...set].sort((a, b) => a - b);
    const runs = [];
    for (const block of blocks) {
      const last = runs[runs.length - 1];
      if (last && last[1] === block - 1) last[1] = block;
      else runs.push([block, block]);
    }
    files[source.paths[entry]] = runs;
  }
  return files;
}

/** One request per map instead of a thousand: the host packs a manifest's
 * blocks into a single file (scripts/web/build-packs.py) with an index of
 * where each run sits. Blocks the store lacks are pulled from the pack in
 * large ranges; whatever the pack does not cover, `prefetch` fetches after. */
const PACK_RANGE = 8 * 1024 * 1024;
let packBytes = 0;
async function prefetchPack(pack) {
  packBytes = 0;
  if (!pack || !store.enabled) return;
  const response = await fetch(pack.index, { cache: "no-store" });
  if (!response.ok) return;
  const index = await response.json();
  if (index.block !== BLOCK) return;
  const byPath = new Map(source.paths.map((path, entry) => [path, entry]));
  // Every missing block as [packOffset, length, entry, block, store], in pack order.
  const pieces = [];
  for (const [path, rows] of Object.entries(index.files || {})) {
    const entry = byPath.get(path);
    if (entry === undefined || !Array.isArray(rows)) continue;
    const s = await storeEntry(entry);
    if (!s) continue;
    const count = blockCount(entry);
    for (const [first, last, offset] of rows) {
      let at = offset;
      for (let block = first; block <= last && block < count; block++) {
        const length = blockLength(entry, block);
        if (!s.present[block]) pieces.push([at, length, entry, block, s]);
        at += length;
      }
    }
  }
  pieces.sort((a, b) => a[0] - b[0]);
  // Contiguous pieces become one range, capped so several run in parallel.
  const ranges = [];
  for (const piece of pieces) {
    const last = ranges[ranges.length - 1];
    if (last && last.end === piece[0] && last.end - last.start < PACK_RANGE) {
      last.end += piece[1];
      last.pieces.push(piece);
    } else {
      ranges.push({ start: piece[0], end: piece[0] + piece[1], pieces: [piece] });
    }
  }
  stats[PREFETCH_DONE] = 0;
  stats[PREFETCH_TOTAL] = ranges.reduce((sum, r) => sum + (r.end - r.start), 0);
  if (!ranges.length) return;
  let next = 0;
  const pull = async () => {
    while (next < ranges.length) {
      const range = ranges[next++];
      let bytes = null;
      for (let attempt = 0; attempt < RETRIES && !bytes; attempt++) {
        try {
          const reply = await fetch(pack.data, {
            headers: { Range: `bytes=${range.start}-${range.end - 1}` },
            cache: "no-store",
          });
          if (reply.status !== 206) throw new Error(`pack range ${reply.status}`);
          const body = new Uint8Array(await reply.arrayBuffer());
          if (body.length === range.end - range.start) bytes = body;
        } catch (error) {
          if (attempt === RETRIES - 1) console.warn("vfs: pack range", error);
          await new Promise((resolve) => setTimeout(resolve, backoff(attempt)));
        }
      }
      if (!bytes) continue;
      const touchedStores = new Set();
      for (const [at, length, entry, block, s] of range.pieces) {
        storeWrite(s, block, bytes.subarray(at - range.start, at - range.start + length));
        touchedStores.add(s);
      }
      for (const s of touchedStores) storeCommit(s);
      stats[PREFETCH_DONE] += bytes.length;
      stats[NET_BYTES] += bytes.length;
      stats[NET_REQUESTS] += 1;
      packBytes += bytes.length;
    }
  };
  await Promise.all(Array.from({ length: 6 }, pull));
}

/** Fetches every listed block this PC does not hold yet, PARALLEL chunks at
 * a time, straight into the store; the engine then finds a map on disk
 * instead of stalling the page on thousands of round trips. */
async function prefetch(files, parallel = PREFETCH_PARALLEL) {
  if (!packBytes) {
    stats[PREFETCH_DONE] = 0;
    stats[PREFETCH_TOTAL] = 0;
  }
  if (!store.enabled) return { bytes: 0, missing: 0 };
  const index = new Map(source.paths.map((path, entry) => [path, entry]));
  const chunks = [];
  // The engine reads the zones it uses whole and probes every zone's header
  // at startup. A zone the manifest shows being read (more than its first
  // few blocks) is taken entirely; a merely probed one, and every other
  // zone, contributes its first block so the probe never reaches the
  // network. Taking probed zones whole once pulled the entire game folder.
  files = { ...files };
  for (const path of source.paths) {
    if (!/\.ff$/i.test(path)) continue;
    const entry = index.get(path);
    if (entry === undefined) continue;
    const runs = Array.isArray(files[path]) ? files[path] : [];
    let listed = 0;
    for (const [first, last] of runs) listed += Math.max(0, last - first + 1);
    const count = blockCount(entry);
    const read = listed > 4 && listed * 4 >= count;
    files[path] = read ? [[0, count - 1]] : [[0, Math.min(3, count - 1)]];
  }
  for (const [path, runs] of Object.entries(files)) {
    const entry = index.get(path);
    if (entry === undefined || !Array.isArray(runs)) continue;
    const s = await storeEntry(entry);
    if (!s) continue;
    const count = blockCount(entry);
    const wanted = new Set();
    for (const [first, last] of runs) {
      for (let block = Math.max(0, first); block <= Math.min(last, count - 1); block++) wanted.add(block);
    }
    // Bridge small gaps between wanted runs so they fetch as one range.
    const sorted = [...wanted].sort((a, b) => a - b);
    for (let i = 1; i < sorted.length; i++) {
      const gap = sorted[i] - sorted[i - 1] - 1;
      if (gap > 0 && gap <= PREFETCH_GAP_BLOCKS) {
        for (let block = sorted[i - 1] + 1; block < sorted[i]; block++) wanted.add(block);
      }
    }
    const missing = [...wanted].filter((block) => !s.present[block]).sort((a, b) => a - b);
    for (const [first, last] of toRuns(missing)) {
      const [start, end] = runBounds(entry, first, last);
      chunks.push([entry, s, first, last]);
      stats[PREFETCH_TOTAL] += end - start;
    }
  }
  let next = 0;
  const pull = async () => {
    while (next < chunks.length) {
      const [entry, s, first, last] = chunks[next++];
      try {
        const bytes = await fetchRunAsync(entry, first, last);
        for (const [block, part] of splitRun(bytes, first)) storeWrite(s, block, part);
        storeCommit(s);
        stats[PREFETCH_DONE] += bytes.length;
      } catch (error) {
        // Reported as missing: the page retries before the engine starts,
        // since the engine would fetch it with the tab blocked.
        const [start, end] = runBounds(entry, first, last);
        missing += end - start;
        console.warn("vfs: prefetch", error);
      }
    }
  };
  let missing = 0;
  await Promise.all(Array.from({ length: Math.max(1, parallel | 0) }, pull));
  const bytes = stats[PREFETCH_DONE];
  packBytes = 0;
  return { bytes, missing };
}

function start(init) {
  ({ control, offset, data, source, stats } = init);
  if (init.engineInWorker) NETWORK_WHILE_SERVING = true;
  control = new Int32Array(control);
  offset = new Float64Array(offset);
  data = new Uint8Array(data);
  SLOTS = Math.max(1, (control.length - 2) >> 2);
  SLOT_WINDOW = Math.floor(data.length / SLOTS);
  DOORBELL = SLOTS * 4;
  stats = stats ? new Float64Array(stats) : new Float64Array(8);
  postMessage("ready");
  if (source.files) {
    serveSync(readFile);
  } else if (navigator.storage?.getDirectory) {
    storeOpen().then(serveAsync);
  } else {
    // Older engines: blocks live in memory for this session only, and the
    // blocking loop below never sees another message.
    stats[STORE_STATE] = -1;
    serveSync(readUrlSync);
  }
}

let started = false;
onmessage = (event) => {
  if (!started) {
    started = true;
    start(event.data);
    return;
  }
  const { type, files, parallel, pack } = event.data;
  if (type === "manifest") {
    postMessage({ type: "manifest", files: manifest() });
  } else if (type === "prefetch") {
    prefetchPack(pack)
      .catch((error) => { console.warn("vfs: pack", error); })
      .then(() => prefetch(files, parallel))
      .then(({ bytes, missing }) => postMessage({ type: "prefetched", bytes, missing }));
  } else if (type === "warm") {
    const { id, entry, at, length } = event.data;
    warm(entry, at, length)
      .catch((error) => console.warn("vfs: warm", error))
      .then(() => postMessage({ type: "warmed", id }));
  }
};
