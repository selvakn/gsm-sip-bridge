/* Spike #111 lab "Agent B": mirrors pjsua-safe/src/endpoint.rs closely enough
 * to test passthrough registration, ringback and switchboard connects.
 *
 *   b [--pt-amr] [--peer <uri>] [--ringback] [--gsm] [--port N] [--secs N]
 *
 * --pt-amr   after pjsua_init: passthrough_deinit + passthrough_init2({AMR})
 * --peer     on an incoming call, answer it and place a call to <uri>; when
 *            both legs have media, conf-connect them both ways (endpoint.rs
 *            ~L949-990)
 * --ringback on CALLING/EARLY of any call, run start_ringback_tone()'s exact
 *            sequence (tonegen -> conf_add_port -> conf_connect(slot,0));
 *            stop on CONFIRMED/DISCONNECTED
 * --gsm      on media active, connect call<->slot 0, adjust_tx_level(0,1.5),
 *            then start a tone into slot 0 and stop it again, reporting
 *            slot 0's transmitter count at each step
 */
#include <pjsua-lib/pjsua.h>
#include <pjmedia-codec.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>

#define THIS_FILE "b"
static pj_str_t peer_uri;
static int opt_ringback, opt_gsm;
static pjsua_call_id in_call = -1, out_call = -1;
static int rb_slot = -1;
static unsigned g_clock = 16000;
static pj_pool_t *rb_pool;

static void say(const char *fmt, ...) {
    va_list ap; va_start(ap, fmt);
    printf("B| "); vprintf(fmt, ap); printf("\n"); fflush(stdout);
    va_end(ap);
}

static void slot_info(const char *what, int slot) {
    pjsua_conf_port_info pi;
    if (pjsua_conf_get_port_info(slot, &pi) != PJ_SUCCESS) { say("%s: slot %d: no info", what, slot); return; }
    say("%s: slot %d '%.*s' fmt=%.4s clock=%u bps=%u listeners=%u", what, slot,
        (int)pi.name.slen, pi.name.ptr, (char*)&pi.format.id, pi.clock_rate,
        pi.format.det.aud.avg_bps, pi.listener_cnt);
}

static int transmitters_into(int sink) {
    /* pjsua has no transmitter count; walk every slot's listeners. */
    pjsua_conf_port_id ids[64]; unsigned n = 64, i, j; int cnt = 0;
    pjsua_enum_conf_ports(ids, &n);
    for (i = 0; i < n; ++i) {
        pjsua_conf_port_info pi;
        if (pjsua_conf_get_port_info(ids[i], &pi) != PJ_SUCCESS) continue;
        for (j = 0; j < pi.listener_cnt; ++j) if (pi.listeners[j] == sink) {
            say("   slot %d -> slot %d", ids[i], sink); ++cnt;
        }
    }
    return cnt;
}

static void ringback_start(void) {
    pjmedia_port *port; pj_status_t st; pjmedia_tone_desc t;
    unsigned clock = g_clock;
    if (rb_slot >= 0) return;
    rb_pool = pjsua_pool_create("ringback", 512, 512);
    st = pjmedia_tonegen_create(rb_pool, clock, 1, clock / 50, 16, 0, &port);
    say("ringback: tonegen_create(clock=%u) -> %d", clock, st);
    if (st) return;
    memset(&t, 0, sizeof t); t.freq1 = 400; t.on_msec = 1000; t.off_msec = 4000;
    pjmedia_tonegen_play(port, 1, &t, PJMEDIA_TONEGEN_LOOP);
    st = pjsua_conf_add_port(rb_pool, port, &rb_slot);
    say("ringback: conf_add_port -> %d slot=%d", st, rb_slot);
    if (st) { rb_slot = -1; return; }
    st = pjsua_conf_connect(rb_slot, 0);
    say("ringback: conf_connect(%d,0) -> %d", rb_slot, st);
    say("ringback: transmitters into slot 0 = %d", transmitters_into(0));
}

static void ringback_stop(void) {
    pj_status_t st;
    if (rb_slot < 0) return;
    st = pjsua_conf_disconnect(rb_slot, 0);
    say("ringback: conf_disconnect(%d,0) -> %d", rb_slot, st);
    st = pjsua_conf_remove_port(rb_slot);
    say("ringback: conf_remove_port -> %d", st);
    rb_slot = -1;
    say("ringback stopped: transmitters into slot 0 = %d", transmitters_into(0));
}

static void on_call_state(pjsua_call_id id, pjsip_event *e) {
    pjsua_call_info ci; PJ_UNUSED_ARG(e);
    pjsua_call_get_info(id, &ci);
    say("call %d state %.*s", id, (int)ci.state_text.slen, ci.state_text.ptr);
    if (!opt_ringback) goto done;
    if (ci.state == PJSIP_INV_STATE_CALLING || ci.state == PJSIP_INV_STATE_EARLY) ringback_start();
    else if (ci.state == PJSIP_INV_STATE_CONFIRMED || ci.state == PJSIP_INV_STATE_DISCONNECTED) ringback_stop();
done:
    if (ci.state == PJSIP_INV_STATE_DISCONNECTED) {
        pjsua_call_id other = (id == in_call) ? out_call : in_call;
        if (other >= 0 && pjsua_call_is_active(other)) pjsua_call_hangup(other, 0, NULL, NULL);
    }
}

static void on_incoming_call(pjsua_acc_id acc, pjsua_call_id id, pjsip_rx_data *rdata) {
    PJ_UNUSED_ARG(rdata);
    in_call = id;
    if (peer_uri.slen) {
        /* Inbound: hold the 200 until the outbound leg answers, as Agent B does. */
        pj_status_t st = pjsua_call_answer(id, 180, NULL, NULL);
        say("answered in-call %d with 180 -> %d", id, st);
        st = pjsua_call_make_call(acc, &peer_uri, NULL, NULL, NULL, &out_call);
        say("make_call(%.*s) -> %d out_call=%d", (int)peer_uri.slen, peer_uri.ptr, st, out_call);
    } else {
        pjsua_call_answer(id, 200, NULL, NULL);
    }
}

static void codec_of(pjsua_call_id id, char *buf, size_t n) {
    pjsua_stream_info si;
    if (pjsua_call_get_stream_info(id, 0, &si) == PJ_SUCCESS)
        snprintf(buf, n, "%.*s/%u fmt=%.4s bps=%u", (int)si.info.aud.fmt.encoding_name.slen,
                 si.info.aud.fmt.encoding_name.ptr, si.info.aud.fmt.clock_rate,
                 (char*)&si.info.aud.param->info.fmt_id, si.info.aud.param->info.avg_bps);
    else snprintf(buf, n, "?");
}

static void on_call_media_state(pjsua_call_id id) {
    pjsua_call_info ci; char c[128];
    pjsua_call_get_info(id, &ci);
    if (ci.media_status != PJSUA_CALL_MEDIA_ACTIVE) return;
    codec_of(id, c, sizeof c);
    say("call %d media active, conf_slot=%d codec=%s", id, ci.conf_slot, c);
    slot_info("  ", ci.conf_slot);

    if (opt_gsm) {
        pj_status_t st;
        st = pjsua_conf_connect(ci.conf_slot, 0); say("gsm: connect(call %d -> 0) -> %d", ci.conf_slot, st);
        st = pjsua_conf_connect(0, ci.conf_slot); say("gsm: connect(0 -> call %d) -> %d", ci.conf_slot, st);
        st = pjsua_conf_adjust_tx_level(0, 1.5f); say("gsm: adjust_tx_level(0,1.5) -> %d", st);
        say("gsm: transmitters into slot 0 = %d", transmitters_into(0));
        say("gsm: --- tone starts while the call is connected to slot 0 ---");
        ringback_start();
        ringback_stop();
        say("gsm: after tone stop, transmitters into slot 0 = %d (call->0 %s)",
            transmitters_into(0), transmitters_into(0) ? "intact" : "LOST");
        return;
    }

    if (peer_uri.slen) {
        pjsua_call_id other = (id == in_call) ? out_call : in_call;
        pjsua_call_info oi;
        if (other < 0 || pjsua_call_get_info(other, &oi) != PJ_SUCCESS ||
            oi.media_status != PJSUA_CALL_MEDIA_ACTIVE || oi.conf_slot < 0) {
            say("call %d: peer not active yet", id);
            return;
        }
        pj_status_t a = pjsua_conf_connect(ci.conf_slot, oi.conf_slot);
        pj_status_t b = pjsua_conf_connect(oi.conf_slot, ci.conf_slot);
        char eb[80];
        pj_strerror(a, eb, sizeof eb);
        say("PAIR connect(%d->%d) -> %d (%s)", ci.conf_slot, oi.conf_slot, a, a ? eb : "ok");
        pj_strerror(b, eb, sizeof eb);
        say("PAIR connect(%d->%d) -> %d (%s)", oi.conf_slot, ci.conf_slot, b, b ? eb : "ok");
        slot_info("  ", oi.conf_slot);
        slot_info("  slot0", 0);
        if (id == out_call && in_call >= 0) {
            /* Outbound leg answered: now complete the inbound 200. */
        }
    }
}

static void on_call_state_answer(pjsua_call_id id, pjsip_event *e) {
    on_call_state(id, e);
    pjsua_call_info ci;
    pjsua_call_get_info(id, &ci);
    if (id == out_call && ci.state == PJSIP_INV_STATE_CONFIRMED && in_call >= 0) {
        pj_status_t st = pjsua_call_answer(in_call, 200, NULL, NULL);
        say("out leg confirmed: answer in-call %d 200 -> %d", in_call, st);
    }
}

static void dump_codecs(const char *when) {
    pjsua_codec_info ci[64]; unsigned n = 64, i;
    pjsua_enum_codecs(ci, &n);
    say("codecs %s (%u):", when, n);
    for (i = 0; i < n; ++i)
        say("   %-24.*s prio=%u", (int)ci[i].codec_id.slen, ci[i].codec_id.ptr, ci[i].priority);
}

int main(int argc, char **argv) {
    pjsua_config cfg; pjsua_logging_config log; pjsua_media_config med;
    pjsua_transport_config tc; pjsua_acc_id acc;
    int i, port = 5070, secs = 20, pt_amr = 0, no_opencore = 0, ptime1 = 0; unsigned clock = 16000; const char *prio_id[8]; int prio_val[8], nprio = 0;
    pj_status_t st;

    for (i = 1; i < argc; ++i) {
        if (!strcmp(argv[i], "--pt-amr")) pt_amr = 1;
        else if (!strcmp(argv[i], "--no-opencore")) no_opencore = 1;
        else if (!strcmp(argv[i], "--ptime20")) ptime1 = 1;
        else if (!strcmp(argv[i], "--clock")) clock = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--prio")) { prio_id[nprio] = argv[++i]; prio_val[nprio++] = atoi(argv[++i]); }
        else if (!strcmp(argv[i], "--peer")) peer_uri = pj_str(argv[++i]);
        else if (!strcmp(argv[i], "--ringback")) opt_ringback = 1;
        else if (!strcmp(argv[i], "--gsm")) opt_gsm = 1;
        else if (!strcmp(argv[i], "--port")) port = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--secs")) secs = atoi(argv[++i]);
    }

    pjsua_create();
    pjsua_config_default(&cfg);
    cfg.cb.on_incoming_call = &on_incoming_call;
    cfg.cb.on_call_state = &on_call_state_answer;
    cfg.cb.on_call_media_state = &on_call_media_state;
    pjsua_logging_config_default(&log);
    log.console_level = 3;
    pjsua_media_config_default(&med);
    med.clock_rate = clock; med.snd_clock_rate = clock; /* Agent B: 16000 */
    g_clock = clock;
    med.no_vad = PJ_TRUE;
    st = pjsua_init(&cfg, &log, &med);
    say("pjsua_init -> %d (switchboard=%d passthrough=%d)", st,
        PJMEDIA_CONF_USE_SWITCH_BOARD, PJMEDIA_HAS_PASSTHROUGH_CODECS);
    if (st) return 1;
    dump_codecs("after pjsua_init");

#if PJMEDIA_HAS_PASSTHROUGH_CODECS
    if (pt_amr) {
        pjmedia_codec_passthrough_setting s;
        pjmedia_format f[2];
        memset(&s, 0, sizeof s); memset(f, 0, sizeof f);
        f[0].id = PJMEDIA_FORMAT_AMR;
        /* No PJMEDIA_FORMAT_AMRWB exists in 2.16 (pjmedia/format.h). */
        s.fmt_cnt = 1; s.fmts = f; s.ilbc_mode = 30;
        if (no_opencore) {
            st = pjmedia_codec_opencore_amr_deinit();
            say("opencore_amr_deinit -> %d", st);
        }
        st = pjmedia_codec_passthrough_deinit();
        say("passthrough_deinit -> %d", st);
        st = pjmedia_codec_passthrough_init2(pjsua_get_pjmedia_endpt(), &s);
        say("passthrough_init2({AMR}) -> %d", st);
        dump_codecs("after passthrough re-init");
        if (ptime1) {
            pjmedia_codec_param cp; pj_str_t id = pj_str("AMR/8000");
            st = pjsua_codec_get_param(&id, &cp);
            say("codec_get_param(AMR/8000) -> %d frm_per_pkt=%u ptime=%u", st, cp.setting.frm_per_pkt, cp.info.frm_ptime);
            cp.setting.frm_per_pkt = 1;
            st = pjsua_codec_set_param(&id, &cp);
            say("codec_set_param(AMR/8000, frm_per_pkt=1) -> %d", st);
        }
    }
#else
    (void)pt_amr;
#endif

    for (i = 0; i < nprio; ++i) {
        pj_str_t id = pj_str((char*)prio_id[i]);
        st = pjsua_codec_set_priority(&id, (pj_uint8_t)prio_val[i]);
        say("codec_set_priority(%s,%d) -> %d", prio_id[i], prio_val[i], st);
    }
    pjsua_transport_config_default(&tc);
    tc.port = port;
    pjsua_transport_create(PJSIP_TRANSPORT_UDP, &tc, NULL);
    pjsua_start();
    st = pjsua_set_null_snd_dev();
    say("set_null_snd_dev -> %d", st);
    pjsua_acc_add_local(0, PJ_TRUE, &acc);
    slot_info("startup slot0", 0);

    for (i = 0; i < secs * 10; ++i) {
        pj_thread_sleep(100);
    }
    if (in_call >= 0 && pjsua_call_is_active(in_call)) pjsua_call_hangup(in_call, 0, NULL, NULL);
    pj_thread_sleep(500);
    pjsua_destroy();
    say("done");
    return 0;
}
