# MW2WEB

Call of Duty: Modern Warfare 2 (2009) multiplayer in a browser tab. MW2WEB is a
fork of [IW4L](https://github.com/vladtrc/iw4L), an open-source MW2 runtime written in
Rust with [Bevy](https://bevy.org/), compiled to WebAssembly and drawn with WebGPU.
The live site is [play.mw2web.com](https://play.mw2web.com); updates are posted on
[X @mw2web_](https://x.com/mw2web_).

**This repository is code only.** It contains no game files and no game art. To run it
you need your own installed copy of MW2 Multiplayer; the engine reads that installation
without patching or replacing its files.

## What it adds to IW4L

| Area | What |
|---|---|
| Browser build | The engine compiled to `wasm32` with threads (Web Workers sharing memory) for Chrome and Edge, and a single-threaded build for Firefox. Rendering goes through WebGPU. |
| Threads | The game simulation runs on its own thread next to the renderer (pipelined), with worker pools for loading, render setup, skinning, effects and audio decode. |
| File streaming | Game files stream from the host in small blocks and are kept in the browser's private file system. Each load records which blocks a map reads, so the next player's download is prefetched in bulk. |
| Lobby | A web lobby (`web/index.html`) over the relay server in `crates/iw4l-master` (`lan` mode): a match list, hosting with passwords and map rotation, Create a Class, and accounts. |
| Counter-Strike maps | `crates/source_map` compiles Hammer `.vmf` maps into the engine's world, collision, lightmaps and entities; `crates/cs2_textures` pulls their textures, props and radar pictures from a Counter-Strike 2 install. |
| Performance | Frame pacing, automatic render resolution, capped pipeline compilation while loading, and many render and simulation optimizations. [PERFORMANCE.md](PERFORMANCE.md) lists each one with its measured effect, and what is still left to do. |

## Build

You need Rust through rustup (the toolchain is pinned in `rust-toolchain.toml`), GNU
Make, and your own MW2 Multiplayer install. See IW4L's [build dependencies](docs/BUILD.md)
for the native toolchain.

**Native**, as upstream:

```bash
cp .env.example .env
# Edit .env: set IW4L_GAMES to the folder containing your game installations.
make map mp_boneyard CMDS='wait world; spawn 0; force_match_start; bot add 3'
```

**Browser** (also needs the `wasm32-unknown-unknown` target, clang for the C crates,
and `wasm-bindgen-cli` matching `Cargo.lock`):

```bash
bash scripts/web/build.sh            # single-threaded bundle -> web/pkg
bash scripts/web/build-threads.sh    # threaded bundle (nightly + rust-src) -> web/pkg-exp
```

**Run it locally**: the relay serves the page, the lobby and your own game folder to
your browser on this machine.

```bash
cargo run --release -p iw4l-master -- lan --bind 127.0.0.1:8444 --http 127.0.0.1:8481 \
  --web web --games "/path/to/Call of Duty Modern Warfare 2"
# then open http://127.0.0.1:8481/?lan=1
```

The page expects fonts and a few images under `web/assets/`, which are not in this
repository; [web/assets/README.md](web/assets/README.md) lists what goes there.

`scripts/web/pack.sh` packs a build for serving, and `scripts/web/deploy-exp.sh`
uploads it to a server named in `scripts/web/deploy.env` (see
`scripts/web/deploy.env.example`).

## Layout

- `crates/`: the engine (asset readers, simulation, rendering, audio, networking) and
  the tools around it.
- `web/`: the lobby page, the docs page and the workers that host the engine.
- `vendor/wgpu`: wgpu with a deferred (`createRenderPipelineAsync`) pipeline path.
- `scripts/web/`: browser build, packing and deploy scripts.
- `docs/`: IW4L's engine documentation.

## Acknowledgements and license

MW2WEB is built on **IW4L by vladtrc** and everyone who contributed to it, including
**ju1cedr1nker** and **silvernote03**, who QA-tested weapons, maps, attachments and
other gameplay features.

[OpenAssetTools](https://github.com/Laupetin/OpenAssetTools) and its [iw4x-x64
fork](https://github.com/iw4x-x64/oat) informed asset layouts;
[IW4x](https://github.com/iw4x/iw4x-client) informed asset and protocol behavior;
[KisakCOD](https://github.com/SwagSoftware/KisakCOD) informed engine structure.
[Ghidra](https://github.com/NationalSecurityAgency/ghidra) was used to inspect the
original binaries. The Counter-Strike map layouts come from
[ReagentX/decompiled-csgo-maps](https://github.com/ReagentX/decompiled-csgo-maps).

The source is licensed under [Apache 2.0](LICENSE). Preserve the attribution in
[NOTICE](NOTICE) and the bundled font licence texts when redistributing. Original game
assets and trademarks belong to their owners (Activision and Infinity Ward for Modern
Warfare 2, Valve for Counter-Strike). MW2WEB is unaffiliated with them.
