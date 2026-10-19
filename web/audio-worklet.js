// The engine's output processor: pulls stereo frames out of a shared ring
// buffer that the page thread renders ahead into (crates/audio/src/web_output.rs).
// Control words: [0] frames written, [1] frames read (wrapping int32), [2]
// quanta filled with silence because the ring ran dry.
class Iw4lOutput extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const { ring, control, frames } = options.processorOptions;
    this.ring = new Float32Array(ring);
    this.control = new Int32Array(control);
    this.frames = frames;
  }

  process(inputs, outputs) {
    const out = outputs[0];
    const left = out[0];
    const right = out.length > 1 ? out[1] : null;
    const n = left.length;
    const written = Atomics.load(this.control, 0);
    const read = Atomics.load(this.control, 1);
    if (((written - read) | 0) < n) {
      left.fill(0);
      if (right) right.fill(0);
      // Silence before the engine's first write is start-up, not a gap:
      // the page attaches this node before the engine boots.
      if (written !== 0) Atomics.add(this.control, 2, 1);
      return true;
    }
    const frames = this.frames;
    let at = ((read % frames) + frames) % frames;
    for (let i = 0; i < n; i++) {
      const index = at * 2;
      left[i] = this.ring[index];
      if (right) right[i] = this.ring[index + 1];
      at++;
      if (at === frames) at = 0;
    }
    Atomics.store(this.control, 1, (read + n) | 0);
    return true;
  }
}

registerProcessor("iw4l-output", Iw4lOutput);
