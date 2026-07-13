/*
 * PJSIP build-time configuration (`pjlib/include/pj/config_site.h`), copied
 * into the pjproject source tree by docker/Dockerfile before it is built.
 *
 * Enable L16 (uncompressed 16-bit PCM) at 16 kHz mono. pjproject registers
 * L16 only at 44.1 kHz by default, and the VoWiFi bridge needs 16 kHz: it is
 * how Agent A hands a carrier's AMR-WB call to Agent B's PJSIP leg over the
 * veth link without narrowing it to 8 kHz first (see
 * `gsm-sip-bridge/src/ims/sdp.rs`, `NegotiatedCodec::L16`). Uncompressed is
 * the point — the veth is a link inside one host, so its 256 kbit/s is free,
 * and there is no codec for Agent A to implement.
 *
 * G.722 (the wideband codec offered to the PBX) needs nothing here: pjproject
 * builds it in by default, with no external library.
 */
#define PJMEDIA_CODEC_L16_HAS_16KHZ_MONO 1
