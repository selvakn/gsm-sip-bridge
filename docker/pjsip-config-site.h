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

/*
 * Drop the local-hostname lookup from `pj_gethostip()`.
 *
 * `pj_gethostip()` gathers address candidates, and its first one is a
 * blocking `pj_getaddrinfo(pj_gethostname())` (`pjlib/src/pj/sock_common.c`).
 * That runs *per call*, not once at startup: with no STUN server and an empty
 * `rtp_cfg.bound_addr` — which is how this project configures PJSUA —
 * `create_rtp_rtcp_sock()` in `pjsip/src/pjsua-lib/pjsua_media.c` calls it
 * while building the media transport, before the SDP answer and so before the
 * 200 OK. Nothing caches the result, and VoWiFi Agent B places two PJSIP calls
 * per bridged call (`gsm-sip-bridge/src/vowifi/mod.rs`), so it is two lookups
 * per call. Wherever the container's hostname is absent from /etc/hosts and
 * the nameserver blackholes the query, that is a 5 s resolver timeout sitting
 * in the answer path — long enough for the carrier or PBX to give up on the
 * INVITE and the call to be rejected.
 *
 * Disabling it costs nothing here. The remaining candidates — the default
 * route's interface plus a full interface enumeration — are what actually gets
 * picked anyway: the hostname candidate on a typical Docker host resolves to
 * the 127.0.1.1 line in /etc/hosts, which `pj_gethostip()` then weights below
 * the default route (WEIGHT_LOOPBACK -5 against WEIGHT_DEF_ROUTE +2).
 *
 * Undocumented upstream — it appears in sock_common.c and in no header, so
 * `pjlib/include/pj/config.h` has no default for it.
 */
#define PJ_GETHOSTIP_DISABLE_LOCAL_RESOLUTION 1
