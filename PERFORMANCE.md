# MW2WEB performance: what's done and what's next

What we changed to make MW2 multiplayer load and run well in a browser tab, with
the measured effect of each change, and what is still left. Unless noted, numbers
were measured in desktop Chrome on a 16-core (32-thread) PC, on Rust or Inferno,
against a local copy of the relay server so other players' traffic didn't skew
the timings.

## Download

| Change | Effect |
|---|---|
| Engine `.wasm` built without its debug-name section, gzipped, and fetched in one streaming request (`scripts/web/pack.sh`) | 114 MB → 16 MB on the wire; the cold engine phase went from 46 s to 37 s |
| Game files streamed in 16 KiB blocks into the browser's private file system (OPFS), down from 64 KiB | ~2,300 small texture reads no longer balloon into 142 MB |
| Each load records which blocks a map reads (a manifest); the server concatenates them into one pack per map (`scripts/web/build-packs.py`) that the page pulls in 8 MiB ranges, 6 at a time (`prefetchPack` in `web/vfs-worker.js`) | Fresh Rust download 28 s → ~10 s. The limit was ~1,100 tiny requests at ~110 ms each, not bandwidth |
| No read-ahead on texture packs (`.iwd`) | Inferno's first load sent 184 MB of a 198 MB texture pack while the game read 45 MB; fixed |
| Two large zone files (114 MB and 51 MB) read once instead of twice per load | Less to download and parse on every load |
| The Textures setting now reaches the browser build (it was ignored, so every load ran at 512 px); Low (128 px) is the default | Smaller downloads and working set |

## Load time

| Change | Effect |
|---|---|
| Loading runs on worker threads (Web Workers sharing memory) | Cold load 35.0 s → 14.5 s; map textures 4.2 s → 0.14 s; first-person weapons 6.4 s → 0.4 s |
| Building all 293 first-person weapons no longer holds up spawning | Warm load 18 s → 14 s |
| Shader pipelines compile at most `max(2, cores / 2)` at a time (`ExactPipelineRegistry::compile_limit` in `crates/render_gpu/src/drawsurf/exact_pipeline.rs`). Chrome used to compile ~1,000 at once on every core | Machine CPU peak during a load 100% → 72%; Chrome's GPU process 74% → 39%; audio decode 7.4 s → 2.0 s. Cost: the load itself takes longer, 13.7 s → 15.1 s here and 15.4 s → 18.6 s with the same PC limited to 8 threads |
| A pipeline that failed to compile now frees its slot | Fixed one cause of Inferno loading forever |

## Frame rate

| Change | Effect |
|---|---|
| Pipelining: the game simulation runs on its own thread and overlaps the renderer (`crates/bootstrap/src/web_worker.rs`); Chrome and Edge only, `?pipeline=0` turns it off | 54–76 fps → 98–158 fps. Per frame: 9.6–14 ms on one thread before; 5.4–8.4 ms of game and 2.8–4.3 ms of rendering in parallel after. Cost: one extra frame of input latency |
| Character skinning, sun-shadow cascades and per-object render setup run on a worker pool (`web_threads::frame_pool`) | Less work on the engine thread each frame |
| Auto resolution: the page lowers the render scale (100/85/70/60/50%) when the graphics card is the limit and tries raising it again later | 2 → 7 fps on a software-only WebGPU adapter; the default in the lobby |
| The engine runs in a Web Worker | Loads no longer freeze the tab |
| Smaller fixes: cached bind-group keys, skipping unchanged spot-shadow uploads, a faster hash in the HUD | A few percent each |

Tried and dropped: whole-program LTO with one codegen unit made frames slower
(42 ms vs 18 ms, likely because V8 doesn't optimize huge functions), `simd128`
changed nothing, and uncapped frames instead of `requestAnimationFrame` were
within noise.

Steady play on Derail: median 4.4 ms per frame, 99th percentile 10.5 ms.

## Stutter and audio

| Change | Effect |
|---|---|
| Sound data is fetched into memory before its first play (`vfs::warm`) | 100–170 ms stalls after kills and explosions → none |
| Audio has its own thread and plays through an AudioWorklet ring buffer (`crates/audio/src/web_output.rs`, `web/audio-worklet.js`) | 0 audio underruns, including through loads |
| The page used to compress a 30–40 MB cache file on the main thread 30 s into every match; it now checks first and skips it when not needed | No mid-match hitch |

## Firefox

Firefox runs the single-threaded build in a worker, with workarounds for its
missing `Atomics.waitAsync` and batched file-system reads. It works, at 28–36 ms per
frame, because its WebGPU is slower.

## Still to build or improve

1. **Memory (the top priority).** A loaded MW2 map takes about 2.3–2.5 GB of the
   tab's memory, and Inferno peaks near 3.3 GB of the 4 GB a WebAssembly page can
   address. That is the crash risk on weaker PCs and the suspected cause of
   Inferno sometimes never finishing its load. About 1 GB is weapon models (on
   Rust at Low textures), nearly half of it first-person models for all 293
   weapons. Loading only the weapons in use is the biggest lever.
2. **Compile on-screen pipelines first.** Pipelines are submitted in hash order;
   submitting the ones the first frame needs first would win back load time lost
   to the compile cap.
3. **Warm load.** Remaining costs: parsing the menu data on every start (1.5 s;
   a cached catalog would cut most of it), uploading 2,584 images one WebGPU call
   at a time (1.5 s), and textures for weapons nobody has equipped.
4. **Per-frame CPU.**
   - Material setup per draw is about 16% of the frame.
   - About 900 sun-shadow draws per frame can't be merged, because each has its
     own constants slot.
   - Skinning runs on the CPU and uploads about 1 MB of vertices per frame;
     skinning on the GPU would remove both.
   - The HUD re-evaluates menu expressions every frame.
5. **Counter-Strike maps.**
   - Materials are colour only (no normal or specular maps), and some textures
     are stand-ins.
   - Props have no collision, and Nuke and Overpass are each missing about 700
     prop instances.
   - The light bake is basic, blend layers show hard edges, and the sky is MW2's.
   - The real fix is importing CS2's compiled maps (world meshes, physics
     collision and baked lighting) instead of converting the old `.vmf` files.
6. **Firefox.** It is slower, and the High (512 px) texture setting has no
   recorded manifest yet, so it streams piece by piece during play.
7. **Fresh maps.** For up to 15 minutes after a map is first played, its pack is
   not built yet, and the page logs bursts of failed range fetches before
   fetching the blocks one by one (the load still completes).
