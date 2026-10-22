# Speech-like test audio, 20 minutes, at 16 kHz and 8 kHz (usage: mkwav.py in16.wav in8.wav).
# Reuses the #111 spike's generator (8 s of AM-modulated harmonics with pauses) and repeats it.
import os, subprocess, sys, tempfile, wave
out16, out8 = sys.argv[1:3]
if not (os.path.exists(out16) and os.path.exists(out8)):
    gen = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "111-amr-passthrough-spike", "harness", "gen_wav.py")
    with tempfile.TemporaryDirectory() as d:
        base = os.path.join(d, "b.wav")
        subprocess.run([sys.executable, "-I", gen, base], check=True)
        w = wave.open(base); p = w.getparams(); data = w.readframes(w.getnframes()); w.close()
    for path, step in ((out16, 1), (out8, 2)):
        samples = [data[i:i + 2] for i in range(0, len(data), 2)][::step]
        o = wave.open(path, "wb"); o.setnchannels(1); o.setsampwidth(2); o.setframerate(p.framerate // step)
        o.writeframes(b"".join(samples) * 150); o.close()
