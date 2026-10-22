/* Spike #111 phase 2 lab "Agent B": PJSIP for signalling only.
 *
 *   b2 [--fwd] [--peer <uri>] [--port N] [--secs N]
 *
 * --fwd   on_create_media_transport swaps every call's UDP media transport for
 *         a pjmedia *loop* transport (rx disabled) that merely reports the
 *         forwarder's address/port, so PJSIP builds its SDP around the
 *         forwarder while its own stream never touches the network. A plain
 *         socket forwarder relays RTP/RTCP between the two legs, rewriting
 *         only the payload type. Without --fwd: today's Agent B, the two call
 *         slots conference-connected (opencore decode + re-encode).
 *
 * The inbound leg is held at 180 until the outbound leg answers, as Agent B
 * does. At exit it prints per-leg PJSIP stream counters (what PJSIP's own
 * stream sent/received) and the process CPU time.
 */
#include <pjsua-lib/pjsua.h>
#include <pjmedia/transport_loop.h>
#include <arpa/inet.h>
#include <netinet/in.h>
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <unistd.h>

static pj_str_t peer_uri;
static int opt_fwd, opt_pause;
static double cpu0;
static double cpu_now(void) { struct rusage r; getrusage(RUSAGE_SELF, &r); return r.ru_utime.tv_sec + r.ru_stime.tv_sec + (r.ru_utime.tv_usec + r.ru_stime.tv_usec) / 1e6; }
static char pub_ip[64] = "127.0.0.1";
static pjsua_call_id in_call = -1, out_call = -1;

struct leg {
    int rtp_fd, rtcp_fd, port;
    int have_peer;
    struct sockaddr_in rem_rtp, rem_rtcp;
    int tx_pt, rx_pt, tx_ev, rx_ev;
    unsigned long rx_pkts, tx_pkts, rtcp_rx, rtcp_tx;
};
static struct leg legs[PJSUA_MAX_CALLS];
static pthread_mutex_t lk = PTHREAD_MUTEX_INITIALIZER;
static pthread_t fwd_thread;
static volatile int fwd_run, fwd_started;

static void say(const char *fmt, ...) {
    va_list ap; va_start(ap, fmt);
    printf("B| "); vprintf(fmt, ap); printf("\n"); fflush(stdout);
    va_end(ap);
}

static int bind_udp(int port) {
    struct sockaddr_in a; socklen_t l = sizeof a;
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    memset(&a, 0, sizeof a); a.sin_family = AF_INET; a.sin_port = htons(port);
    if (bind(fd, (struct sockaddr*)&a, sizeof a) < 0) { close(fd); return -1; }
    getsockname(fd, (struct sockaddr*)&a, &l);
    return fd;
}
static int port_of(int fd) {
    struct sockaddr_in a; socklen_t l = sizeof a;
    getsockname(fd, (struct sockaddr*)&a, &l); return ntohs(a.sin_port);
}

/* The hook: replace PJSIP's media transport with one that never sends. */
static pjmedia_transport *on_create_media_transport(pjsua_call_id id, unsigned mi,
                                                    pjmedia_transport *base_tp, unsigned flags) {
    pjmedia_loop_tp_setting s; pjmedia_transport *tp; pj_status_t st;
    struct leg *L = &legs[id];
    PJ_UNUSED_ARG(flags);
    {   pjsua_call_info ci;
        pj_status_t gs = pjsua_call_get_info(id, &ci);
        say("call %d: in on_create_media_transport: get_info -> %d role=%s call-id=%.*s remote=%.*s media[%u].type=%d",
            id, gs, ci.role == PJSIP_ROLE_UAS ? "UAS" : "UAC", (int)ci.call_id.slen, ci.call_id.ptr,
            (int)ci.remote_info.slen, ci.remote_info.ptr, mi, gs == PJ_SUCCESS && mi < ci.media_cnt ? (int)ci.media[mi].type : -1);
    }
    if (!opt_fwd || mi != 0) return base_tp;
    for (;;) {                       /* an even RTP port with RTCP on port+1 */
        int a = bind_udp(0), p = port_of(a), b;
        if (p % 2 == 0 && (b = bind_udp(p + 1)) >= 0) { L->rtp_fd = a; L->rtcp_fd = b; L->port = p; break; }
        close(a);
    }
    pjmedia_loop_tp_setting_default(&s);
    s.af = pj_AF_INET();
    s.addr = pj_str(pub_ip);
    s.port = L->port;
    s.disable_rx = PJ_TRUE;
    st = pjmedia_transport_loop_create2(pjsua_get_pjmedia_endpt(), &s, &tp);
    say("call %d: forwarder bound %s:%d/%d; loop_create2 -> %d; closing PJSIP's own transport",
        id, pub_ip, L->port, L->port + 1, st);
    if (st != PJ_SUCCESS) return base_tp;
    pjmedia_transport_close(base_tp);
    return tp;
}

static void relay(struct leg *src, struct leg *dst, int rtcp) {
    unsigned char buf[2048]; ssize_t n;
    n = recv(rtcp ? src->rtcp_fd : src->rtp_fd, buf, sizeof buf, MSG_DONTWAIT);
    if (n <= 0) return;
    pthread_mutex_lock(&lk);
    if (!dst->have_peer) { pthread_mutex_unlock(&lk); return; }
    if (rtcp) {
        src->rtcp_rx++; dst->rtcp_tx++;
        sendto(dst->rtcp_fd, buf, n, 0, (struct sockaddr*)&dst->rem_rtcp, sizeof dst->rem_rtcp);
    } else if (n >= 12) {
        int pt = buf[1] & 0x7f;
        /* Payload type is per leg: map what src's peer sends to what dst's peer expects. */
        if (pt == src->rx_pt) pt = dst->tx_pt;
        else if (src->rx_ev >= 0 && pt == src->rx_ev && dst->tx_ev >= 0) pt = dst->tx_ev;
        buf[1] = (buf[1] & 0x80) | pt;
        src->rx_pkts++; dst->tx_pkts++;
        sendto(dst->rtp_fd, buf, n, 0, (struct sockaddr*)&dst->rem_rtp, sizeof dst->rem_rtp);
    }
    pthread_mutex_unlock(&lk);
}

static void *forwarder(void *arg) {
    struct leg *a = &legs[in_call], *b = &legs[out_call];
    struct pollfd p[4] = {{a->rtp_fd, POLLIN, 0}, {a->rtcp_fd, POLLIN, 0},
                          {b->rtp_fd, POLLIN, 0}, {b->rtcp_fd, POLLIN, 0}};
    PJ_UNUSED_ARG(arg);
    while (fwd_run) {
        if (poll(p, 4, 100) <= 0) continue;
        if (p[0].revents) relay(a, b, 0);
        if (p[1].revents) relay(a, b, 1);
        if (p[2].revents) relay(b, a, 0);
        if (p[3].revents) relay(b, a, 1);
    }
    return NULL;
}

static void learn_peer(pjsua_call_id id) {
    pjsua_stream_info si; struct leg *L = &legs[id]; char ra[64];
    if (pjsua_call_get_stream_info(id, 0, &si) != PJ_SUCCESS) return;
    pthread_mutex_lock(&lk);
    memcpy(&L->rem_rtp, &si.info.aud.rem_addr.ipv4, sizeof L->rem_rtp);
    memcpy(&L->rem_rtcp, &si.info.aud.rem_rtcp.ipv4, sizeof L->rem_rtcp);
    L->tx_pt = si.info.aud.tx_pt; L->rx_pt = si.info.aud.rx_pt;
    L->tx_ev = si.info.aud.tx_event_pt; L->rx_ev = si.info.aud.rx_event_pt;
    L->have_peer = 1;
    pthread_mutex_unlock(&lk);
    pj_sockaddr_print(&si.info.aud.rem_addr, ra, sizeof ra, 3);
    say("call %d: codec %.*s/%u, remote RTP %s, pt tx=%d rx=%d, tel-event tx=%d rx=%d",
        id, (int)si.info.aud.fmt.encoding_name.slen, si.info.aud.fmt.encoding_name.ptr,
        si.info.aud.fmt.clock_rate, ra, L->tx_pt, L->rx_pt, L->tx_ev, L->rx_ev);
}

static void on_call_media_state(pjsua_call_id id) {
    pjsua_call_info ci;
    pjsua_call_get_info(id, &ci);
    say("call %d media_status=%d conf_slot=%d", id, ci.media_status, ci.conf_slot);
    if (ci.media_status != PJSUA_CALL_MEDIA_ACTIVE && ci.media_status != PJSUA_CALL_MEDIA_REMOTE_HOLD &&
        ci.media_status != PJSUA_CALL_MEDIA_LOCAL_HOLD) return;
    if (opt_fwd) {
        learn_peer(id);      /* every media update (re-INVITE/UPDATE) re-learns the peer */
        if (!fwd_started && in_call >= 0 && out_call >= 0 && legs[in_call].have_peer && legs[out_call].have_peer) {
            fwd_run = 1; fwd_started = 1;
            pthread_create(&fwd_thread, NULL, forwarder, NULL);
            say("forwarder started (no conference connection made)"); cpu0 = cpu_now();
        }
        return;
    }
    {   /* baseline: today's Agent B pairing */
        pjsua_call_id other = (id == in_call) ? out_call : in_call;
        pjsua_call_info oi;
        if (other < 0 || pjsua_call_get_info(other, &oi) != PJ_SUCCESS ||
            oi.media_status != PJSUA_CALL_MEDIA_ACTIVE || oi.conf_slot < 0 || ci.conf_slot < 0) return;
        pjsua_conf_connect(ci.conf_slot, oi.conf_slot);
        pjsua_conf_connect(oi.conf_slot, ci.conf_slot);
        say("baseline: conference-connected %d <-> %d", ci.conf_slot, oi.conf_slot);
        if (cpu0 == 0) cpu0 = cpu_now();
    }
}

static void on_call_state(pjsua_call_id id, pjsip_event *e) {
    pjsua_call_info ci; PJ_UNUSED_ARG(e);
    pjsua_call_get_info(id, &ci);
    say("call %d state %.*s", id, (int)ci.state_text.slen, ci.state_text.ptr);
    if (id == out_call && ci.state == PJSIP_INV_STATE_CONFIRMED && in_call >= 0)
        say("answer in-call 200 -> %d", pjsua_call_answer(in_call, 200, NULL, NULL));
    if (ci.state == PJSIP_INV_STATE_DISCONNECTED) {
        pjsua_call_id other = (id == in_call) ? out_call : in_call;
        if (other >= 0 && pjsua_call_is_active(other)) pjsua_call_hangup(other, 0, NULL, NULL);
    }
}

static void on_incoming_call(pjsua_acc_id acc, pjsua_call_id id, pjsip_rx_data *rdata) {
    PJ_UNUSED_ARG(rdata);
    in_call = id;
    say("answer in-call %d 180 -> %d", id, pjsua_call_answer(id, 180, NULL, NULL));
    { pjsua_call_setting cs; pjsua_call_setting_default(&cs); cs.aud_cnt = 1; cs.vid_cnt = 0; cs.txt_cnt = 0; /* as pjsua-safe/src/call.rs */
      say("make_call -> %d", pjsua_call_make_call(acc, &peer_uri, &cs, NULL, NULL, &out_call)); }
}

static void on_stream_created2(pjsua_call_id id, pjsua_on_stream_created_param *prm) {
    if (opt_fwd && opt_pause)
        say("call %d: stream created; pjmedia_stream_pause(ENC+DEC) -> %d", id,
            pjmedia_stream_pause(prm->stream, PJMEDIA_DIR_ENCODING_DECODING));
}

static void on_call_sdp_created(pjsua_call_id id, pjmedia_sdp_session *sdp, pj_pool_t *pool,
                                const pjmedia_sdp_session *rem) {
    /* Observation only: log the c=/m= PJSIP is about to send. */
    unsigned i; PJ_UNUSED_ARG(pool);
    for (i = 0; i < sdp->media_count; ++i) {
        pjmedia_sdp_conn *c = sdp->media[i]->conn ? sdp->media[i]->conn : sdp->conn;
        say("call %d local SDP (%s): c=%.*s m=%.*s %u", id, rem ? "answer" : "offer",
            (int)c->addr.slen, c->addr.ptr, (int)sdp->media[i]->desc.media.slen,
            sdp->media[i]->desc.media.ptr, sdp->media[i]->desc.port);
    }
}

static void stats(pjsua_call_id id, const char *name) {
    pjsua_stream_stat ss;
    if (id < 0 || pjsua_call_get_stream_stat(id, 0, &ss) != PJ_SUCCESS) { say("%s: no stream stat", name); return; }
    say("%s: PJSIP stream sent rtp=%u, received rtp=%u | forwarder rx=%lu tx=%lu rtcp rx=%lu tx=%lu",
        name, ss.rtcp.tx.pkt, ss.rtcp.rx.pkt, legs[id].rx_pkts, legs[id].tx_pkts, legs[id].rtcp_rx, legs[id].rtcp_tx);
}

int main(int argc, char **argv) {
    pjsua_config cfg; pjsua_logging_config log; pjsua_media_config med;
    pjsua_transport_config tc; pjsua_transport_id tid; pjsua_transport_info ti;
    pjsua_acc_id acc; struct rusage ru;
    int i, port = 5070, secs = 14;

    for (i = 0; i < PJSUA_MAX_CALLS; ++i) legs[i].rtp_fd = legs[i].rtcp_fd = -1;
    for (i = 1; i < argc; ++i) {
        if (!strcmp(argv[i], "--fwd")) opt_fwd = 1;
        else if (!strcmp(argv[i], "--pause")) opt_pause = 1;
        else if (!strcmp(argv[i], "--peer")) peer_uri = pj_str(argv[++i]);
        else if (!strcmp(argv[i], "--port")) port = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--secs")) secs = atoi(argv[++i]);
    }
    pjsua_create();
    pjsua_config_default(&cfg);
    cfg.cb.on_incoming_call = &on_incoming_call;
    cfg.cb.on_call_state = &on_call_state;
    cfg.cb.on_call_media_state = &on_call_media_state;
    cfg.cb.on_create_media_transport = &on_create_media_transport;
    cfg.cb.on_call_sdp_created = &on_call_sdp_created;
    cfg.cb.on_stream_created2 = &on_stream_created2;
    pjsua_logging_config_default(&log); log.console_level = 3;
    pjsua_media_config_default(&med);
    med.clock_rate = med.snd_clock_rate = 16000; med.no_vad = PJ_TRUE;
    if (pjsua_init(&cfg, &log, &med) != PJ_SUCCESS) return 1;
    pjsua_transport_config_default(&tc); tc.port = port;
    pjsua_transport_create(PJSIP_TRANSPORT_UDP, &tc, &tid);
    pjsua_transport_get_info(tid, &ti);
    snprintf(pub_ip, sizeof pub_ip, "%.*s", (int)ti.local_name.host.slen, ti.local_name.host.ptr);
    pjsua_start();
    pjsua_set_null_snd_dev();
    pjsua_acc_add_local(tid, PJ_TRUE, &acc);
    say("mode=%s published=%s", opt_fwd ? "signalling-only+forwarder" : "baseline (conference bridge)", pub_ip);

    for (i = 0; i < secs * 10; ++i) {
        pj_thread_sleep(100);
        if (i == 90) { stats(in_call, "in-leg (X)"); stats(out_call, "out-leg (Y)");
            say("CPU during call window: %.1f ms", (cpu_now() - cpu0) * 1000); }
    }
    getrusage(RUSAGE_SELF, &ru);
    say("CPU user=%ld.%03lds sys=%ld.%03lds", ru.ru_utime.tv_sec, ru.ru_utime.tv_usec / 1000,
        ru.ru_stime.tv_sec, ru.ru_stime.tv_usec / 1000);
    fwd_run = 0; if (fwd_started) pthread_join(fwd_thread, NULL);
    pjsua_destroy();
    return 0;
}
