#include "bridge/bridge_config.h"
#include "bridge/bridge_account.h"
#include "bridge/bridge_call.h"
#include "bridge/alsa_media_port.h"
#include "bridge/beep_generator.h"
#include "ring_buffer.h"
#include "sip/sip_config.h"
#include "at_commander.h"
#include "audio_loop.h"
#include "device_discovery.h"
#include "logger.h"
#include "serial_port.h"

#include <pjsua2.hpp>
#include <alsa/asoundlib.h>

#include <atomic>
#include <chrono>
#include <csignal>
#include <cstdlib>
#include <cstring>
#include <getopt.h>
#include <string>
#include <thread>
#include <vector>

static constexpr const char* VERSION = "0.1.0";
static constexpr const char* DEFAULT_CONFIG_PATH = "config.ini";

static constexpr unsigned int SAMPLE_RATE = 8000;
static constexpr unsigned int CHANNELS = 1;
static constexpr unsigned int PERIOD_FRAMES = 160;
static constexpr unsigned int BUFFER_FRAMES = 640;
static constexpr size_t RING_BUFFER_FRAMES = 4000;

static constexpr unsigned int ERROR_TONE_HZ = 200;
static constexpr unsigned int ERROR_TONE_ON_MS = 500;
static constexpr unsigned int ERROR_TONE_OFF_MS = 100;
static constexpr unsigned int ERROR_TONE_DURATION_MS = 2000;

static std::atomic<bool> g_running{true};

static void signal_handler(int) {
    g_running.store(false, std::memory_order_relaxed);
}

enum class BridgeState {
    IDLE,
    GSM_ANSWERED,
    SIP_DIALING,
    BRIDGED,
    SIP_FAILED,
    ENDING
};


struct CliArgs {
    std::string config_path = DEFAULT_CONFIG_PATH;
    std::string serial_override;
    std::string audio_override;
    bool verbose = false;
};

static int parse_args(int argc, char* argv[], CliArgs& args) {
    static struct option long_opts[] = {
        {"config",  required_argument, nullptr, 'c'},
        {"serial",  required_argument, nullptr, 's'},
        {"audio",   required_argument, nullptr, 'a'},
        {"verbose", no_argument,       nullptr, 'v'},
        {"help",    no_argument,       nullptr, 'h'},
        {"version", no_argument,       nullptr, 'V'},
        {nullptr, 0, nullptr, 0}
    };

    int opt;
    while ((opt = getopt_long(argc, argv, "c:s:a:vh", long_opts, nullptr)) != -1) {
        switch (opt) {
            case 'c': args.config_path = optarg;    break;
            case 's': args.serial_override = optarg; break;
            case 'a': args.audio_override = optarg;  break;
            case 'v': args.verbose = true;           break;
            case 'h':
                std::printf(
                    "Usage: %s [OPTIONS]\n\n"
                    "GSM to SIP audio bridge. Answers incoming GSM calls and\n"
                    "bridges audio to a SIP extension.\n\n"
                    "Options:\n"
                    "  -c, --config PATH   Configuration file (default: %s)\n"
                    "  -s, --serial PATH   Override serial port (default: auto-detect)\n"
                    "  -a, --audio DEVICE  Override ALSA device (default: auto-detect)\n"
                    "  -v, --verbose       Enable verbose logging\n"
                    "  -h, --help          Show this help\n"
                    "      --version       Show version\n",
                    argv[0], DEFAULT_CONFIG_PATH);
                return -1;
            case 'V':
                std::printf("gsm-sip-bridge %s\n", VERSION);
                return -1;
            default:
                return 1;
        }
    }
    return 0;
}

static DeviceInfo resolve_device(const CliArgs& args) {
    DeviceInfo info;

    if (!args.serial_override.empty() && !args.audio_override.empty()) {
        info.serial_port = args.serial_override;
        info.alsa_device = args.audio_override;
        LOG_INFO("manual overrides: serial=%s, audio=%s",
                 info.serial_port.c_str(), info.alsa_device.c_str());
        return info;
    }

    auto detected = discover_ec20();
    if (!detected) {
        LOG_ERROR("no EC20 module found (USB %04X:%04X)", EC20_VENDOR_ID, EC20_PRODUCT_ID);
        return {};
    }

    info = *detected;
    if (!args.serial_override.empty()) info.serial_port = args.serial_override;
    if (!args.audio_override.empty()) info.alsa_device = args.audio_override;
    return info;
}

struct AlsaPcm {
    snd_pcm_t* capture = nullptr;
    snd_pcm_t* playback = nullptr;
};

static bool configure_pcm(snd_pcm_t* pcm, const char* label) {
    snd_pcm_hw_params_t* hw;
    snd_pcm_hw_params_alloca(&hw);
    snd_pcm_hw_params_any(pcm, hw);

    int err;
    if ((err = snd_pcm_hw_params_set_access(pcm, hw, SND_PCM_ACCESS_RW_INTERLEAVED)) < 0) {
        LOG_ERROR("%s: set access: %s", label, snd_strerror(err));
        return false;
    }
    if ((err = snd_pcm_hw_params_set_format(pcm, hw, SND_PCM_FORMAT_S16_LE)) < 0) {
        LOG_ERROR("%s: set format: %s", label, snd_strerror(err));
        return false;
    }
    unsigned int ch = CHANNELS;
    if ((err = snd_pcm_hw_params_set_channels(pcm, hw, ch)) < 0) {
        LOG_ERROR("%s: set channels: %s", label, snd_strerror(err));
        return false;
    }
    unsigned int rate = SAMPLE_RATE;
    if ((err = snd_pcm_hw_params_set_rate_near(pcm, hw, &rate, nullptr)) < 0) {
        LOG_ERROR("%s: set rate: %s", label, snd_strerror(err));
        return false;
    }
    snd_pcm_uframes_t period = PERIOD_FRAMES;
    if ((err = snd_pcm_hw_params_set_period_size_near(pcm, hw, &period, nullptr)) < 0) {
        LOG_ERROR("%s: set period: %s", label, snd_strerror(err));
        return false;
    }
    snd_pcm_uframes_t buffer = BUFFER_FRAMES;
    if ((err = snd_pcm_hw_params_set_buffer_size_near(pcm, hw, &buffer)) < 0) {
        LOG_ERROR("%s: set buffer: %s", label, snd_strerror(err));
        return false;
    }
    if ((err = snd_pcm_hw_params(pcm, hw)) < 0) {
        LOG_ERROR("%s: apply params: %s", label, snd_strerror(err));
        return false;
    }
    if ((err = snd_pcm_prepare(pcm)) < 0) {
        LOG_ERROR("%s: prepare: %s", label, snd_strerror(err));
        return false;
    }
    return true;
}

static bool open_alsa(const std::string& device, AlsaPcm& pcm) {
    int err;
    if ((err = snd_pcm_open(&pcm.capture, device.c_str(), SND_PCM_STREAM_CAPTURE, 0)) < 0) {
        LOG_ERROR("ALSA capture open: %s", snd_strerror(err));
        return false;
    }
    if ((err = snd_pcm_open(&pcm.playback, device.c_str(), SND_PCM_STREAM_PLAYBACK, 0)) < 0) {
        LOG_ERROR("ALSA playback open: %s", snd_strerror(err));
        snd_pcm_close(pcm.capture);
        pcm.capture = nullptr;
        return false;
    }
    if (!configure_pcm(pcm.capture, "capture") || !configure_pcm(pcm.playback, "playback")) {
        snd_pcm_close(pcm.capture);
        snd_pcm_close(pcm.playback);
        pcm = {};
        return false;
    }
    LOG_INFO("ALSA opened: %s (rate=%u, period=%u, buffer=%u)",
             device.c_str(), SAMPLE_RATE, PERIOD_FRAMES, BUFFER_FRAMES);
    return true;
}

static void close_alsa(AlsaPcm& pcm) {
    if (pcm.capture) { snd_pcm_drop(pcm.capture); snd_pcm_close(pcm.capture); }
    if (pcm.playback) { snd_pcm_drop(pcm.playback); snd_pcm_close(pcm.playback); }
    pcm = {};
}

static void alsa_capture_thread(snd_pcm_t* capture,
                                RingBuffer<int16_t>& ring,
                                std::atomic<bool>& running,
                                std::atomic<bool>& bridged) {
    std::vector<int16_t> buf(PERIOD_FRAMES);
    bool was_bridged = false;
    while (running.load(std::memory_order_relaxed)) {
        snd_pcm_sframes_t frames = snd_pcm_readi(capture, buf.data(), PERIOD_FRAMES);
        if (frames < 0) {
            if (frames == -EPIPE) {
                snd_pcm_prepare(capture);
                continue;
            }
            if (frames == -EINTR) continue;
            LOG_ERROR("ALSA capture: %s", snd_strerror(static_cast<int>(frames)));
            break;
        }

        bool now_bridged = bridged.load(std::memory_order_acquire);
        if (now_bridged && !was_bridged) {
            ring.reset();
            was_bridged = true;
        }

        if (was_bridged) {
            ring.try_write(buf.data(), static_cast<size_t>(frames));
        }
    }
}

static void alsa_playback_thread(snd_pcm_t* playback,
                                 RingBuffer<int16_t>& ring,
                                 BeepGenerator& beep,
                                 std::atomic<bool>& running,
                                 std::atomic<bool>& beep_active,
                                 std::atomic<bool>& bridged) {
    std::vector<int16_t> buf(PERIOD_FRAMES);
    std::vector<int16_t> silence(PERIOD_FRAMES, 0);

    unsigned int prefill = (BUFFER_FRAMES / PERIOD_FRAMES);
    if (prefill > 1) prefill -= 1;
    for (unsigned int i = 0; i < prefill; ++i) {
        snd_pcm_writei(playback, silence.data(), PERIOD_FRAMES);
    }

    bool was_bridged = false;

    while (running.load(std::memory_order_relaxed)) {
        bool now_bridged = bridged.load(std::memory_order_acquire);
        if (now_bridged && !was_bridged) {
            ring.reset();
            was_bridged = true;
        }

        if (beep_active.load(std::memory_order_acquire)) {
            beep.fill_frame(buf.data(), PERIOD_FRAMES);
        } else {
            size_t got = ring.read(buf.data(), PERIOD_FRAMES);
            if (got < PERIOD_FRAMES) {
                std::memset(buf.data() + got, 0,
                            (PERIOD_FRAMES - got) * sizeof(int16_t));
            }
        }

        snd_pcm_sframes_t written = snd_pcm_writei(playback, buf.data(), PERIOD_FRAMES);
        if (written < 0) {
            if (written == -EPIPE) {
                snd_pcm_prepare(playback);
                for (unsigned int i = 0; i + 1 < prefill; ++i) {
                    snd_pcm_writei(playback, silence.data(), PERIOD_FRAMES);
                }
                snd_pcm_writei(playback, buf.data(), PERIOD_FRAMES);
                continue;
            }
            if (written == -EINTR) continue;
            LOG_ERROR("ALSA playback: %s", snd_strerror(static_cast<int>(written)));
            break;
        }
    }
}

static void play_error_tone(snd_pcm_t* playback) {
    BeepGenerator error_beep(ERROR_TONE_HZ, ERROR_TONE_ON_MS, ERROR_TONE_OFF_MS, SAMPLE_RATE);
    unsigned int total_samples = SAMPLE_RATE * ERROR_TONE_DURATION_MS / 1000;
    std::vector<int16_t> buf(PERIOD_FRAMES);

    unsigned int played = 0;
    while (played < total_samples) {
        error_beep.fill_frame(buf.data(), PERIOD_FRAMES);
        snd_pcm_sframes_t written = snd_pcm_writei(playback, buf.data(), PERIOD_FRAMES);
        if (written < 0) {
            if (written == -EPIPE) {
                snd_pcm_prepare(playback);
                snd_pcm_writei(playback, buf.data(), PERIOD_FRAMES);
            }
        }
        played += PERIOD_FRAMES;
    }
    snd_pcm_drain(playback);
}

static void handle_bridged_call(AtCommander& at,
                                const std::string& alsa_device,
                                BridgeAccount& account,
                                const std::string& sip_dest_uri,
                                uint16_t dial_timeout_sec,
                                const std::string& gsm_caller_id) {
    AlsaPcm alsa{};
    if (!open_alsa(alsa_device, alsa)) {
        LOG_ERROR("ALSA open failed, hanging up GSM");
        at.hangup();
        return;
    }

    RingBuffer<int16_t> capture_ring(RING_BUFFER_FRAMES);
    RingBuffer<int16_t> playback_ring(RING_BUFFER_FRAMES);
    BeepGenerator beep;
    std::atomic<bool> audio_running{true};
    std::atomic<bool> beep_active{true};
    std::atomic<bool> audio_bridged{false};

    std::thread cap_thread(alsa_capture_thread, alsa.capture,
                           std::ref(capture_ring), std::ref(audio_running),
                           std::ref(audio_bridged));
    std::thread play_thread(alsa_playback_thread, alsa.playback,
                            std::ref(playback_ring), std::ref(beep),
                            std::ref(audio_running), std::ref(beep_active),
                            std::ref(audio_bridged));

    LOG_INFO("beep pattern started for GSM caller");

    BridgeCall* sip_call = account.make_outbound_call(sip_dest_uri, gsm_caller_id);
    if (!sip_call) {
        LOG_ERROR("SIP call initiation failed");
        beep_active.store(false, std::memory_order_release);
        audio_running.store(false, std::memory_order_relaxed);
        cap_thread.join();
        play_thread.join();
        snd_pcm_drop(alsa.playback);
        snd_pcm_prepare(alsa.playback);
        play_error_tone(alsa.playback);
        close_alsa(alsa);
        at.hangup();
        account.clear_call();
        return;
    }

    AlsaMediaPort media_port(capture_ring, playback_ring);
    try {
        media_port.create();
    } catch (pj::Error& err) {
        LOG_ERROR("media port creation failed: %s", err.info().c_str());
        account.hangup_call();
        account.clear_call();
        beep_active.store(false, std::memory_order_release);
        audio_running.store(false, std::memory_order_relaxed);
        cap_thread.join();
        play_thread.join();
        close_alsa(alsa);
        at.hangup();
        return;
    }

    BridgeState state = BridgeState::SIP_DIALING;

    auto dial_start = std::chrono::steady_clock::now();
    bool bridge_connected = false;

    while (g_running.load(std::memory_order_relaxed)) {
        auto urc = at.poll_urc();
        if (urc) {
            if (urc->find("NO CARRIER") != std::string::npos ||
                urc->find("BUSY") != std::string::npos ||
                urc->find("NO ANSWER") != std::string::npos) {
                LOG_INFO("GSM call ended (remote hangup)");
                state = BridgeState::ENDING;
                break;
            }
        }

        SipCallState sip_state = sip_call->sip_state();

        if (state == BridgeState::SIP_DIALING || state == BridgeState::GSM_ANSWERED) {
            auto elapsed = std::chrono::duration_cast<std::chrono::seconds>(
                std::chrono::steady_clock::now() - dial_start).count();

            if (elapsed >= dial_timeout_sec) {
                LOG_WARN("SIP dial timeout (%us)", dial_timeout_sec);
                state = BridgeState::SIP_FAILED;
                break;
            }

            if (sip_state == SipCallState::FAILED) {
                LOG_WARN("SIP call failed");
                state = BridgeState::SIP_FAILED;
                break;
            }

            if (sip_state == SipCallState::CONFIRMED && sip_call->media_connected()) {
                beep_active.store(false, std::memory_order_release);
                audio_bridged.store(true, std::memory_order_release);

                try {
                    pj::CallInfo ci = sip_call->getInfo();
                    for (unsigned i = 0; i < ci.media.size(); ++i) {
                        if (ci.media[i].type != PJMEDIA_TYPE_AUDIO) continue;
                        if (ci.media[i].status != PJSUA_CALL_MEDIA_ACTIVE) continue;

                        pj::AudioMedia aud = sip_call->getAudioMedia(i);
                        media_port.startTransmit(aud);
                        aud.startTransmit(media_port);
                        bridge_connected = true;
                        LOG_INFO("audio bridge connected (GSM <-> SIP)");
                        break;
                    }
                } catch (pj::Error& err) {
                    LOG_ERROR("media connect failed: %s", err.info().c_str());
                    state = BridgeState::SIP_FAILED;
                    break;
                }

                state = BridgeState::BRIDGED;
            }
        }

        if (state == BridgeState::BRIDGED) {
            if (sip_state == SipCallState::DISCONNECTED) {
                LOG_INFO("SIP party hung up");
                state = BridgeState::ENDING;
                break;
            }
        }

        std::this_thread::sleep_for(std::chrono::milliseconds(20));
    }

    beep_active.store(false, std::memory_order_release);
    audio_running.store(false, std::memory_order_relaxed);

    if (bridge_connected) {
        try {
            pj::CallInfo ci = sip_call->getInfo();
            for (unsigned i = 0; i < ci.media.size(); ++i) {
                if (ci.media[i].type != PJMEDIA_TYPE_AUDIO) continue;
                if (ci.media[i].status != PJSUA_CALL_MEDIA_ACTIVE) continue;
                pj::AudioMedia aud = sip_call->getAudioMedia(i);
                media_port.stopTransmit(aud);
                aud.stopTransmit(media_port);
                break;
            }
        } catch (...) {}
    }

    account.hangup_call();

    cap_thread.join();
    play_thread.join();

    if (state == BridgeState::SIP_FAILED) {
        LOG_INFO("playing error tone to GSM caller");
        snd_pcm_drop(alsa.playback);
        snd_pcm_prepare(alsa.playback);
        play_error_tone(alsa.playback);
    }

    close_alsa(alsa);
    at.hangup();
    account.clear_call();

    LOG_INFO("call teardown complete, state: IDLE");
}

int main(int argc, char* argv[]) {
    CliArgs args;
    int parse_result = parse_args(argc, argv, args);
    if (parse_result != 0) return parse_result < 0 ? 0 : parse_result;

    LOG_INFO("gsm-sip-bridge v%s starting", VERSION);

    struct sigaction sa{};
    sa.sa_handler = signal_handler;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGINT, &sa, nullptr);
    sigaction(SIGTERM, &sa, nullptr);

    SipConfig sip_config;
    auto sip_result = SipConfig::load(args.config_path, sip_config);
    if (!sip_result.ok) {
        LOG_ERROR("SIP config: %s", sip_result.error.c_str());
        return 3;
    }

    BridgeConfig bridge_config;
    auto bridge_result = BridgeConfig::load(args.config_path, bridge_config);
    if (!bridge_result.ok) {
        LOG_ERROR("bridge config: %s", bridge_result.error.c_str());
        return 3;
    }

    LOG_INFO("config loaded: SIP %s@%s, bridge dest=%s, timeout=%us",
             sip_config.username.c_str(), sip_config.server.c_str(),
             bridge_config.sip_destination.c_str(), bridge_config.sip_dial_timeout_sec);

    if (std::system("systemctl is-active --quiet ModemManager 2>/dev/null") == 0) {
        LOG_WARN("ModemManager is running and may interfere with serial access");
    }

    DeviceInfo device = resolve_device(args);
    if (device.serial_port.empty() || device.alsa_device.empty()) {
        return 1;
    }

    SerialPort serial;
    if (!serial.open(device.serial_port)) {
        return 2;
    }

    AtCommander at(serial);
    at.set_verbose(args.verbose);

    for (int i = 0; i < 20; ++i) {
        if (!serial.read_line()) break;
    }

    if (!at.send_and_expect_ok("ATE0", 2000)) {
        at.send_and_expect_ok("ATE0", 2000);
    }
    at.send_and_expect_ok("AT+CLIP=1", 2000);

    if (!at.send_and_expect_ok("AT+QPCMV=1,2", 2000)) {
        LOG_WARN("AT+QPCMV=1,2 failed, trying AT+QPCMV=1,0");
        at.send_and_expect_ok("AT+QPCMV=1,0", 2000);
    }

    if (!at.query_network_registration()) {
        LOG_ERROR("SIM not registered on network");
        return 6;
    }
    LOG_INFO("GSM network registration confirmed");

    pj::Endpoint ep;
    try {
        ep.libCreate();

        pj::EpConfig ep_cfg;
        if (!args.verbose) {
            ep_cfg.logConfig.level = 0;
            ep_cfg.logConfig.consoleLevel = 0;
        } else {
            ep_cfg.logConfig.level = 4;
            ep_cfg.logConfig.consoleLevel = 4;
        }
        ep_cfg.uaConfig.userAgent = "gsm-sip-bridge/" + std::string(VERSION);
        ep.libInit(ep_cfg);

        pj::TransportConfig tp_cfg;
        tp_cfg.port = sip_config.local_port;

        pjsip_transport_type_e tp_type = PJSIP_TRANSPORT_UDP;
        if (sip_config.transport == "tcp") tp_type = PJSIP_TRANSPORT_TCP;
        else if (sip_config.transport == "tls") tp_type = PJSIP_TRANSPORT_TLS;

        ep.transportCreate(tp_type, tp_cfg);
        ep.libStart();
        ep.audDevManager().setNullDev();

        LOG_INFO("PJSIP started (transport=%s, local_port=%u)",
                 sip_config.transport.c_str(), sip_config.local_port);

    } catch (pj::Error& err) {
        LOG_ERROR("PJSIP init: %s", err.info().c_str());
        return 4;
    }

    BridgeAccount account;
    try {
        pj::AccountConfig acc_cfg;
        acc_cfg.idUri = "\"" + sip_config.display_name + "\" <" + sip_config.sip_uri() + ">";
        acc_cfg.regConfig.registrarUri = sip_config.registrar_uri();

        pj::AuthCredInfo cred("digest", "*", sip_config.username, 0, sip_config.password);
        acc_cfg.sipConfig.authCreds.push_back(cred);

        acc_cfg.regConfig.timeoutSec = 300;
        acc_cfg.regConfig.retryIntervalSec = 30;

        acc_cfg.natConfig.contactRewriteUse = 1;
        acc_cfg.natConfig.contactRewriteMethod = 2;
        acc_cfg.natConfig.sdpNatRewriteUse = 1;
        acc_cfg.natConfig.sipOutboundUse = 0;

        account.create(acc_cfg);
        LOG_INFO("SIP registering as %s@%s:%u",
                 sip_config.username.c_str(), sip_config.server.c_str(), sip_config.port);

    } catch (pj::Error& err) {
        LOG_ERROR("SIP account: %s", err.info().c_str());
        return 5;
    }

    std::string sip_dest_uri = "sip:" + bridge_config.sip_destination
                               + "@" + sip_config.server
                               + ":" + std::to_string(sip_config.port);

    LOG_INFO("ready, GSM calls will bridge to SIP %s (timeout=%us)",
             sip_dest_uri.c_str(), bridge_config.sip_dial_timeout_sec);

    static constexpr int CLIP_WAIT_MS = 300;

    while (g_running.load(std::memory_order_relaxed)) {
        auto urc = at.poll_urc();
        if (!urc) continue;

        std::string caller;

        if (urc->find("+CLIP:") != std::string::npos) {
            auto start = urc->find('"');
            auto end = urc->find('"', start + 1);
            if (start != std::string::npos && end != std::string::npos) {
                caller = urc->substr(start + 1, end - start - 1);
            }
        }

        if (urc->find("RING") != std::string::npos) {
            if (caller.empty()) {
                auto clip_deadline = std::chrono::steady_clock::now()
                    + std::chrono::milliseconds(CLIP_WAIT_MS);
                while (std::chrono::steady_clock::now() < clip_deadline) {
                    auto clip_urc = at.poll_urc();
                    if (!clip_urc) continue;
                    if (clip_urc->find("+CLIP:") != std::string::npos) {
                        auto start = clip_urc->find('"');
                        auto end = clip_urc->find('"', start + 1);
                        if (start != std::string::npos && end != std::string::npos) {
                            caller = clip_urc->substr(start + 1, end - start - 1);
                        }
                        break;
                    }
                }
            }

            LOG_INFO("GSM RING%s%s", caller.empty() ? "" : " from ",
                     caller.empty() ? "" : caller.c_str());

            if (!account.is_registered()) {
                LOG_WARN("SIP not registered, ignoring GSM call");
                at.hangup();
                continue;
            }

            if (at.answer_call()) {
                LOG_INFO("GSM call answered, bridging to %s (caller: %s)",
                         sip_dest_uri.c_str(),
                         caller.empty() ? "unknown" : caller.c_str());
                handle_bridged_call(at, device.alsa_device, account,
                                    sip_dest_uri, bridge_config.sip_dial_timeout_sec,
                                    caller);
                LOG_INFO("idle, waiting for next GSM call");
            } else {
                LOG_ERROR("failed to answer GSM call");
            }
        }
    }

    LOG_INFO("shutting down");
    at.hangup();

    try {
        account.hangup_call();
        account.shutdown();
        std::this_thread::sleep_for(std::chrono::milliseconds(500));
        ep.libDestroy();
    } catch (pj::Error& err) {
        LOG_ERROR("shutdown: %s", err.info().c_str());
    }

    serial.close();
    LOG_INFO("gsm-sip-bridge stopped");
    return 0;
}
