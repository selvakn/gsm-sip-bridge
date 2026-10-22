# 8 s of speech-like 16 kHz mono audio: AM-modulated harmonics with pauses,
# so the AMR encoder emits both speech frames and (in the gaps) SID/NO_DATA.
import math, struct, sys, wave
rate, secs = 16000, 8
w = wave.open(sys.argv[1], "wb"); w.setnchannels(1); w.setsampwidth(2); w.setframerate(rate)
out = bytearray()
for n in range(rate * secs):
    t = n / rate
    on = (int(t * 2) % 3) != 2                       # 1 s talk, 0.5 s pause
    f0 = 140 + 40 * math.sin(2 * math.pi * 0.7 * t)
    env = 0.5 * (1 + math.sin(2 * math.pi * 4 * t))
    v = sum(math.sin(2 * math.pi * f0 * k * t) / k for k in range(1, 8)) * env if on else 0.0
    out += struct.pack("<h", int(max(-1, min(1, v * 0.25)) * 32767))
w.writeframes(out); w.close()
