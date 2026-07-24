# Audio Tuning Log

Track changes to modem (EC20) and SIP stack audio parameters over time.
All AT+QEEC values use the **EC20 Series index mapping** (Table 6 of AT+EEC_Manual_V1.2).

Note: entries below predate the config reorg that split `[audio]` into
`[audio]` (shared) and `[modem_audio]` (circuit-switched only, `rx_gain`/
`eec_mode`/`tx_level`/`rt_audio_prio`) — see `docs/migrating-config-reorg.md`.
Historical entries still say `config.toml [audio]`; read that as
`[modem_audio]` for those four keys under the current config shape.

---

## Baseline — 2026-05-30 (v5.5.3)

**Deployed version:** `ghcr.io/selvakn/gsm-sip-bridge:5.5.3`

### config.toml [audio]
| Parameter    | Value  | Notes                         |
|--------------|--------|-------------------------------|
| profile      | "lan"  |                               |
| vad          | false  | VAD disabled in PJMEDIA       |
| rx_gain      | 2000   | AT+QRXGAIN — downlink digital gain (SIP→GSM earpiece) |
| tx_level     | 1.4    | PJSUA conf bridge tx level (GSM→SIP amplification)    |

### AT+QRXGAIN / AT+QMIC / AT+CLVL
| Command      | Value         | Notes                                          |
|--------------|---------------|------------------------------------------------|
| AT+QRXGAIN   | 2000          | Range 0–65535; default NV for USB audio mode  |
| AT+QMIC      | 20577, 14567  | Uplink mic gain (main mic, sub mic)            |
| AT+CLVL      | 3             | Loudspeaker/earpiece volume level (0–5)        |

### AT+QEEC? — Full snapshot (EC20 index mapping)
| Index | Parameter           | Value  | Doc Default / Rec.       | Status    |
|-------|---------------------|--------|--------------------------|-----------|
| 0     | NLPP_limit          | 2048   | 32767 (decrease in 1024 steps) | ⚠ Low — heavy Rx clipper |
| 1     | NLPP_gain           | 2048   | 2048 (0 dB)              | OK        |
| 2     | Mode                | 12543  | Do not change            | OK (default) |
| 3     | tuning_mode         | 0      | 0                        | OK        |
| 4     | AF_limit            | 32767  | 32767                    | OK        |
| 5     | echo_path_delay     | 14     | 14                       | OK        |
| 6     | Outputgain          | 2048   | 2048 (0 dB)              | OK        |
| 7     | Inputgain           | 8192   | 8192 (0 dB)              | OK        |
| 8     | AF_twoalpha         | 8192   | 8192                     | OK        |
| 9     | AF_erl              | 250    | ~250                     | OK        |
| 10    | AF_taps             | 160    | 160 (mode-specific)      | OK        |
| 11    | AF_preset_coefs     | 2      | 2                        | OK        |
| 12    | AF_offset           | 767    | 767                      | OK        |
| 13    | AF_erl_bg           | 64     | 64                       | OK        |
| 14    | AF_taps_bg          | 32     | 32                       | OK        |
| 15    | PCD_threshold       | 18000  | 18000                    | OK        |
| 16    | minimum_erl         | 64     | 64                       | OK        |
| 17    | erl_step            | 18000  | mode-specific            | OK        |
| 18    | SPDET_far           | 20000  | 20000                    | OK        |
| 19    | SPDET_mic           | 20000  | 20000                    | OK        |
| 20    | SPDET_xclip         | 512    | mode-specific            | OK        |
| 21    | DENS_tail_alpha     | 25000  | Aggressive=25000, Medium=19000, Least=16000 | ⚠ Aggressive |
| 22    | DENS_tail_portion   | 6000   | Aggressive=12000, Medium=6000, Least=3000  | Medium    |
| 23    | DENS_gamma_e_alpha  | 0      | > 200                    | ⚠ Very low |
| 24    | DENS_gamma_e_high   | 600    | Aggressive=768, Medium=600, Least=450      | Medium    |
| 25    | DENS_gamma_e_dt     | 256    | > 200                    | OK        |
| 26    | DENS_gamma_e_low    | 256    | Do not change            | OK        |
| 27    | DENS_gamma_e_rescue | 1024   | > 200                    | OK        |
| 28    | DENS_spdet_near     | 1024   | mode-specific            | OK        |
| 29    | DENS_spdet_act      | 768    | Do not change            | OK        |
| 30    | DENS_gamma_n        | 600    | Do not change            | OK        |
| 31    | DENS_NFE_blocksize  | 200    | 400 (= 4 sec)            | ⚠ Half of recommended |
| 32    | DENS_limit_NS       | 4628   | -10dB=10349, -13dB=7336, -15dB=5827 | ⚠ More aggressive than -15dB |
| 33    | DENS_NL_atten       | 768    | 896 (Aggressive), 768 (Medium), 640 (Least) | Medium |
| 34    | DENS_CNI_level      | 4096   | 12000                    | ⚠ Low    |
| 35    | WB_echo_ratio       | 4096   | 4000                     | OK        |
| 36    | WB_gamma_n          | 768    | mode-specific            | OK        |
| 37    | WB_gamma_e          | 768    | mode-specific            | OK        |
| 38    | max_noise_floor     | 2048   | 2048                     | OK        |
| 39    | det_threshold       | 99     | 99                       | OK        |
| 40    | WB_tail_alpha       | 6000   | mode-specific            | OK        |
| 41    | WB_tail_portion     | 4000   | mode-specific            | OK        |
| 42    | AF_PostGain         | 2048   | mode-specific            | OK        |
| 43    | AF_High_limit       | 32767  | mode-specific            | OK        |
| 44    | AF_High_taps        | 80     | mode-specific            | OK        |
| 45    | AF_High_twoalpha    | 8192   | 8192                     | OK        |
| 46    | AF_High_erl         | 512    | mode-specific            | OK        |
| 47    | AF_High_offset      | 767    | 767                      | OK        |
| 48    | WB_Echo_Scale       | 0      | mode-specific            | OK        |
| 49    | Rx_Ref_Gain         | 8192   | mode-specific            | OK        |
| 50    | (undocumented)      | 1      | —                        | —         |

**Issue under investigation:** Male voice GSM caller heard with high noise; female voice OK.
**Hypothesis:** Over-aggressive noise suppression (`DENS_limit_NS=4628`, below -15dB) producing musical noise artefacts, amplified by `NLPP_limit=2048` (under-sized Rx clipper impeding EEC reference signal).

---

## Change Log

<!-- Append entries below as changes are made. Format:
### YYYY-MM-DD — Brief description
**Reason:** ...
| Parameter | Index | Old | New | AT Command |
|-----------|-------|-----|-----|------------|
-->

### 2026-05-30 — Reduce noise suppression aggressiveness

**Reason:** Male voice GSM caller heard with high noise. `DENS_limit_NS=4628` is more aggressive than -15dB (doc table: -15dB=5827). Over-suppression produces musical noise artefacts that are more audible on low-pitched male voices.

| Parameter    | Index | Old  | New  | AT Command           |
|--------------|-------|------|------|----------------------|
| DENS_limit_NS | 32   | 4628 | 7336 | AT+QEEC=32,7336      |

**Note:** Setting is volatile — resets on modem power cycle. If it improves quality, it needs to be persisted (e.g. sent via config.toml or on-startup AT command).

**Status:** Applied. Superseded by next change (full EC disable test).

### 2026-05-30 — Disable full echo canceller

**Reason:** USB audio bridge has no acoustic echo path, so the EC is only introducing artefacts (noise suppression distortion on male voices). Full disable is safe to test.

| Parameter | Index | Old   | New | AT Command      |
|-----------|-------|-------|-----|-----------------|
| Mode      | 2     | 12543 | 0   | AT+QEEC=2,0     |

**Note:** Volatile — resets on modem power cycle.

**Status:** Applied. Awaiting test call feedback.

### 2026-05-30 — Persist EC disable in config + code

**Reason:** Test call confirmed full EC disable improved quality. Wired `eec_mode = 0` into `config.toml` as `audio.eec_mode` and added `AT+QEEC=2,<val>` startup command in code so it applies on every card init and after scheduled restarts.

| Parameter | Index | Old   | New | Config key | AT Command  |
|-----------|-------|-------|-----|------------|-------------|
| Mode      | 2     | 12543 | 0   | eec_mode   | AT+QEEC=2,0 |

**Deployed version:** v5.5.4

### 2026-05-31 — Increase Uplink (MIC) Gain

**Reason:** After dropping QMIC to 8192, the noise was gone but the GSM caller's voice was too quiet. Increasing to 14567 (a middle ground between 8192 and the original 20577).

| Parameter | Old | New | AT Command |
|-----------|-----|-----|------------|
| QMIC      | 8192,8192 | 14567,14567 | AT+QMIC=14567,14567 |

**Note:** Applied temporarily via diagnostic port for testing.
**Status:** Applied. Superseded by baseline revert.

### 2026-05-31 — Fix Sample Rate Mismatch (AT+QDAI)

**Reason:** The GSM caller's audio had massive high-frequency noise (2000-4000 Hz) that occurred intermittently. Analysis revealed the modem's Digital Audio Interface (`AT+QDAI`) was set to 16 kHz (`+QDAI: 1,0,0,4,0,1,1,1`), but the USB audio interface only exposes 8 kHz to ALSA. When a VoLTE/HD Voice call negotiated 16 kHz AMR-WB, the sample rate mismatch caused severe aliasing distortion. Changed the internal PCM sample rate to 8 kHz to match the USB interface.

| Parameter | Old | New | AT Command |
|-----------|-----|-----|------------|
| QDAI (sample rate) | 1 (16 kHz) | 0 (8 kHz) | AT+QDAI=1,0,0,4,0,0,1,1 |

**Note:** Applied temporarily via diagnostic port for testing. May require a modem reboot to fully take effect.
**Status:** Applied. Awaiting test call feedback.

