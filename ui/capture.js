class Capture extends AudioWorkletProcessor {
  buffer = new DataView(new ArrayBuffer(4096));
  length = 0;
  peak = 0;
  capturing = true;

  constructor() {
    super();
    this.port.onmessage = ({data}) => {
      if (data !== "stop") return;
      this.capturing = false;
      this.flush();
      this.port.postMessage({ stopped: true });
    };
  }

  flush() {
    if (!this.length) return;
    const pcm = this.buffer.buffer.slice(0, this.length * 2);
    this.port.postMessage({ pcm, peak: this.peak }, [pcm]);
    this.length = 0;
    this.peak = 0;
  }

  process(inputs) {
    const channel = inputs[0]?.[0];
    if (!channel || !this.capturing) return true;
    for (const value of channel) {
      const sample = Math.max(-1, Math.min(1, value));
      this.peak = Math.max(this.peak, Math.abs(sample));
      this.buffer.setInt16(this.length++ * 2, sample * (sample < 0 ? 32768 : 32767), true);
      if (this.length === 2048) this.flush();
    }
    return true;
  }
}
registerProcessor("capture-pcm", Capture);
