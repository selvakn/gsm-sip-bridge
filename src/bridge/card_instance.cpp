#include "bridge/card_instance.h"
#include "bridge/bridge_account.h"
#include "bridge/bridge_call.h"
#include "bridge/alsa_media_port.h"
#include "bridge/beep_generator.h"
#include "bridge/metrics.h"
#include "bridge/sms_handler.h"
#include "ring_buffer.h"
#include "sip/sip_config.h"
#include "logger.h"

#include <pjsua2.hpp>
#include <pj/os.h>
#include <alsa/asoundlib.h>

#include <chrono>
#include <cstdlib>
#include <cstring>
#include <thread>
#include <vector>

static constexpr unsigned int SAMPLE_RATE = 8000;
static constexpr unsigned int CHANNELS = 1;
static constexpr unsigned int PERIOD_FRAMES = 160;
static constexpr unsigned int BUFFER_FRAMES = 640;
static constexpr size_t RING_BUFFER_FRAMES = 4000;

static constexpr unsigned int ERROR_TONE_HZ = 200;
static constexpr unsigned int ERROR_TONE_ON_MS = 500;
static constexpr unsigned int ERROR_TONE_OFF_MS = 100;
static constexpr unsigned int ERROR_TONE_DURATION_MS = 2000;

static constexpr int CLIP_WAIT_MS = 300;
static constexpr size_t CARD_ID_SUFFIX_LEN = 6;
static constexpr int MODULE_REBOOT_WAIT_SEC = 15;
static constexpr int MODULE_REBOOT_POLL_SEC = 2;
static constexpr int MODULE_REBOOT_MAX_RETRIES = 10;

const char* card_state_str(CardState state) {
    switch (state) {
        case CardState::DISCOVERED:   return "DISCOVERED";
        case CardState::INITIALIZING: return "INITIALIZING";
        case CardState::ACTIVE:       return "ACTIVE";
        case CardState::FAILED:       return "FAILED";
        case CardState::STOPPING:     return "STOPPING";
        case CardState::STOPPED:      return "STOPPED";
    }
    return "UNKNOWN";
}

std::string derive_card_id(const std::string& serial_number, const std::string& usb_path) {
    if (!serial_number.empty()) {
        std::string suffix = serial_number;
        if (suffix.size() > CARD_ID_SUFFIX_LEN) {
            suffix = suffix.substr(suffix.size() - CARD_ID_SUFFIX_LEN);
        }
        return "ec20-" + suffix;
    }
    return "ec20-" + usb_path;
}

CardInstance::CardInstance(DeviceInfo device)
    : device_(std::move(device)),
      card_id_(derive_card_id(device_.serial_number, device_.usb_path)) {}

CardInstance::~CardInstance() {
    stop();
}

bool CardInstance::initialize(bool verbose) {
    verbose_ = verbose;
    state_.store(CardState::INITIALIZING, std::memory_order_release);

    LOG_INFO("[%s] initializing (serial=%s, audio=%s)",
             card_id_.c_str(), device_.serial_port.c_str(), device_.alsa_device.c_str());

    if (!serial_.open(device_.serial_port)) {
        fail_reason_ = "serial port open failed: " + device_.serial_port;
        LOG_ERROR("[%s] %s", card_id_.c_str(), fail_reason_.c_str());
        state_.store(CardState::FAILED, std::memory_order_release);
        return false;
    }

    at_ = std::make_unique<AtCommander>(serial_);
    at_->set_verbose(verbose);

    for (int i = 0; i < 20; ++i) {
        if (!serial_.read_line()) break;
    }

    if (!at_->send_and_expect_ok("ATE0", 2000)) {
        at_->send_and_expect_ok("ATE0", 2000);
    }

    const char* reset_env = std::getenv("GSM_RESET_ON_START");
    if (reset_env && std::string(reset_env) == "1") {
        LOG_INFO("[%s] GSM_RESET_ON_START=1, rebooting module via AT+CFUN=1,1",
                 card_id_.c_str());
        at_->send("AT+CFUN=1,1");
        serial_.close();
        at_.reset();

        LOG_INFO("[%s] waiting %ds for module to reboot...",
                 card_id_.c_str(), MODULE_REBOOT_WAIT_SEC);
        std::this_thread::sleep_for(std::chrono::seconds(MODULE_REBOOT_WAIT_SEC));

        bool reconnected = false;
        for (int attempt = 0; attempt < MODULE_REBOOT_MAX_RETRIES; ++attempt) {
            auto refreshed = discover_all_ec20();
            for (auto& dev : refreshed) {
                if (dev.serial_number == device_.serial_number) {
                    if (dev.serial_port != device_.serial_port ||
                        dev.alsa_device != device_.alsa_device) {
                        LOG_INFO("[%s] device paths changed after reboot: "
                                 "serial %s->%s, audio %s->%s",
                                 card_id_.c_str(),
                                 device_.serial_port.c_str(), dev.serial_port.c_str(),
                                 device_.alsa_device.c_str(), dev.alsa_device.c_str());
                    }
                    device_.serial_port = dev.serial_port;
                    device_.alsa_device = dev.alsa_device;
                    device_.usb_path = dev.usb_path;
                    break;
                }
            }

            if (serial_.open(device_.serial_port)) {
                at_ = std::make_unique<AtCommander>(serial_);
                at_->set_verbose(verbose);
                for (int i = 0; i < 20; ++i) {
                    if (!serial_.read_line()) break;
                }
                if (at_->send_and_expect_ok("ATE0", 2000)) {
                    reconnected = true;
                    LOG_INFO("[%s] module back online after reboot (serial=%s, audio=%s)",
                             card_id_.c_str(),
                             device_.serial_port.c_str(),
                             device_.alsa_device.c_str());
                    break;
                }
                serial_.close();
                at_.reset();
            }
            LOG_INFO("[%s] module not ready, retrying in %ds (%d/%d)",
                     card_id_.c_str(), MODULE_REBOOT_POLL_SEC,
                     attempt + 1, MODULE_REBOOT_MAX_RETRIES);
            std::this_thread::sleep_for(std::chrono::seconds(MODULE_REBOOT_POLL_SEC));
        }

        if (!reconnected) {
            fail_reason_ = "module did not come back after reboot";
            LOG_ERROR("[%s] %s", card_id_.c_str(), fail_reason_.c_str());
            state_.store(CardState::FAILED, std::memory_order_release);
            return false;
        }
    }

    at_->send_and_expect_ok("AT+CLIP=1", 2000);

    if (!at_->send_and_expect_ok("AT+QPCMV=1,2", 2000)) {
        LOG_WARN("[%s] AT+QPCMV=1,2 failed, trying AT+QPCMV=1,0", card_id_.c_str());
        at_->send_and_expect_ok("AT+QPCMV=1,0", 2000);
    }

    if (!at_->query_network_registration()) {
        fail_reason_ = "SIM not registered on network";
        LOG_ERROR("[%s] %s", card_id_.c_str(), fail_reason_.c_str());
        serial_.close();
        at_.reset();
        state_.store(CardState::FAILED, std::memory_order_release);
        return false;
    }

    LOG_INFO("[%s] GSM network registration confirmed", card_id_.c_str());

    if (at_->send("AT+CNUM")) {
        auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(3000);
        while (std::chrono::steady_clock::now() < deadline) {
            auto line = at_->poll_urc();
            if (!line) continue;
            if (line->find("+CNUM:") != std::string::npos) {
                auto q1 = line->find('"', line->find("+CNUM:"));
                if (q1 != std::string::npos) {
                    auto q2 = line->find('"', q1 + 1);
                    auto q3 = line->find('"', q2 + 1);
                    auto q4 = line->find('"', q3 + 1);
                    if (q3 != std::string::npos && q4 != std::string::npos) {
                        own_number_ = line->substr(q3 + 1, q4 - q3 - 1);
                    }
                }
            }
            if (*line == "OK" || line->find("ERROR") != std::string::npos) break;
        }
    }

    if (!own_number_.empty()) {
        LOG_INFO("[%s] SIM number: %s", card_id_.c_str(), own_number_.c_str());
    } else {
        LOG_WARN("[%s] SIM number not available (AT+CNUM empty)", card_id_.c_str());
    }

    state_.store(CardState::ACTIVE, std::memory_order_release);
    return true;
}

void CardInstance::start(BridgeAccount& account,
                         const BridgeConfig& bridge_config,
                         const SipConfig& sip_config,
                         std::atomic<bool>& running,
                         SmsHandler* sms_handler) {
    thread_ = std::thread(&CardInstance::run_loop, this,
                          std::ref(account), std::cref(bridge_config),
                          std::cref(sip_config), std::ref(running),
                          sms_handler);
}

void CardInstance::stop() {
    CardState expected = CardState::ACTIVE;
    state_.compare_exchange_strong(expected, CardState::STOPPING, std::memory_order_release);

    if (thread_.joinable()) {
        thread_.join();
    }

    if (at_) {
        at_->hangup();
    }

    serial_.close();
    at_.reset();
    state_.store(CardState::STOPPED, std::memory_order_release);
}

// ALSA helpers (same logic as original main.cpp)

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
                snd_pcm_start(capture);
                continue;
            }
            if (frames == -EINTR) continue;
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

enum class BridgeState {
    IDLE,
    GSM_ANSWERED,
    SIP_DIALING,
    BRIDGED,
    SIP_FAILED,
    ENDING
};

void CardInstance::handle_bridged_call(AtCommander& at,
                                      BridgeAccount& account,
                                      const std::string& sip_dest_uri,
                                      uint16_t dial_timeout_sec,
                                      const std::string& gsm_caller_id,
                                      std::atomic<bool>& running) {
    AlsaPcm alsa{};
    if (!open_alsa(device_.alsa_device, alsa)) {
        LOG_ERROR("[%s] ALSA open failed, hanging up GSM", card_id_.c_str());
        metrics::audio_error(card_id_, "alsa_open");
        at.hangup();
        return;
    }
    metrics::active_calls_inc(card_id_);

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

    LOG_INFO("[%s] beep pattern started for GSM caller", card_id_.c_str());

    metrics::sip_call_initiated(card_id_);
    BridgeCall* sip_call = account.make_outbound_call(sip_dest_uri, gsm_caller_id);
    if (!sip_call) {
        LOG_ERROR("[%s] SIP call initiation failed", card_id_.c_str());
        metrics::sip_call_failed(card_id_, "initiation_error");
        metrics::active_calls_dec(card_id_);
        beep_active.store(false, std::memory_order_release);
        audio_running.store(false, std::memory_order_relaxed);
        cap_thread.join();
        play_thread.join();
        snd_pcm_drop(alsa.playback);
        snd_pcm_prepare(alsa.playback);
        play_error_tone(alsa.playback);
        close_alsa(alsa);
        at.hangup();
        return;
    }

    int sip_call_id = sip_call->getId();

    AlsaMediaPort media_port(capture_ring, playback_ring);
    try {
        media_port.create();
    } catch (pj::Error& err) {
        LOG_ERROR("[%s] media port creation failed: %s", card_id_.c_str(), err.info().c_str());
        account.hangup_call(sip_call_id);
        account.remove_call(sip_call_id);
        beep_active.store(false, std::memory_order_release);
        audio_running.store(false, std::memory_order_relaxed);
        cap_thread.join();
        play_thread.join();
        close_alsa(alsa);
        at.hangup();
        return;
    }

    BridgeState state = BridgeState::SIP_DIALING;
    auto call_start_time = std::chrono::steady_clock::now();
    auto dial_start = std::chrono::steady_clock::now();
    bool bridge_connected = false;
    unsigned int last_media_version = 0;

    auto connect_media = [&]() -> bool {
        try {
            pj::CallInfo ci = sip_call->getInfo();
            for (unsigned i = 0; i < ci.media.size(); ++i) {
                if (ci.media[i].type != PJMEDIA_TYPE_AUDIO) continue;
                if (ci.media[i].status != PJSUA_CALL_MEDIA_ACTIVE) continue;

                pj::AudioMedia aud = sip_call->getAudioMedia(i);
                media_port.startTransmit(aud);
                aud.startTransmit(media_port);
                return true;
            }
        } catch (pj::Error& err) {
            LOG_ERROR("[%s] media connect failed: %s", card_id_.c_str(), err.info().c_str());
        }
        return false;
    };

    while (running.load(std::memory_order_relaxed)) {
        auto urc = at.poll_urc();
        if (urc) {
            if (urc->find("NO CARRIER") != std::string::npos ||
                urc->find("BUSY") != std::string::npos ||
                urc->find("NO ANSWER") != std::string::npos) {
                LOG_INFO("[%s] GSM call ended (remote hangup)", card_id_.c_str());
                state = BridgeState::ENDING;
                break;
            }
        }

        SipCallState sip_state = sip_call->sip_state();

        if (state == BridgeState::SIP_DIALING || state == BridgeState::GSM_ANSWERED) {
            auto elapsed = std::chrono::duration_cast<std::chrono::seconds>(
                std::chrono::steady_clock::now() - dial_start).count();

            if (elapsed >= dial_timeout_sec) {
                LOG_WARN("[%s] SIP dial timeout (%us)", card_id_.c_str(), dial_timeout_sec);
                metrics::sip_call_failed(card_id_, "timeout");
                state = BridgeState::SIP_FAILED;
                break;
            }

            if (sip_state == SipCallState::FAILED) {
                LOG_WARN("[%s] SIP call failed", card_id_.c_str());
                metrics::sip_call_failed(card_id_, "error");
                state = BridgeState::SIP_FAILED;
                break;
            }

            if (sip_state == SipCallState::CONFIRMED && sip_call->media_connected()) {
                beep_active.store(false, std::memory_order_release);
                audio_bridged.store(true, std::memory_order_release);

                if (connect_media()) {
                    bridge_connected = true;
                    last_media_version = sip_call->media_version();
                    metrics::sip_call_connected(card_id_);
                    LOG_INFO("[%s] audio bridge connected (GSM <-> SIP)", card_id_.c_str());
                } else {
                    metrics::sip_call_failed(card_id_, "media_error");
                    state = BridgeState::SIP_FAILED;
                    break;
                }

                state = BridgeState::BRIDGED;
            }
        }

        if (state == BridgeState::BRIDGED) {
            if (sip_state == SipCallState::DISCONNECTED) {
                LOG_INFO("[%s] SIP party hung up", card_id_.c_str());
                state = BridgeState::ENDING;
                break;
            }

            unsigned int cur_version = sip_call->media_version();
            if (cur_version != last_media_version) {
                LOG_INFO("[%s] SIP media renegotiated, reconnecting bridge", card_id_.c_str());
                if (connect_media()) {
                    last_media_version = cur_version;
                } else {
                    state = BridgeState::SIP_FAILED;
                    break;
                }
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

    account.hangup_call(sip_call_id);

    cap_thread.join();
    play_thread.join();

    if (state == BridgeState::SIP_FAILED) {
        LOG_INFO("[%s] playing error tone to GSM caller", card_id_.c_str());
        snd_pcm_drop(alsa.playback);
        snd_pcm_prepare(alsa.playback);
        play_error_tone(alsa.playback);
    }

    close_alsa(alsa);
    at.hangup();
    account.remove_call(sip_call_id);

    double call_duration = std::chrono::duration<double>(
        std::chrono::steady_clock::now() - call_start_time).count();
    metrics::call_ended(card_id_, call_duration);
    metrics::active_calls_dec(card_id_);

    LOG_INFO("[%s] call teardown complete (duration=%.1fs)", card_id_.c_str(), call_duration);
}

void CardInstance::run_loop(BridgeAccount& account,
                            const BridgeConfig& bridge_config,
                            const SipConfig& sip_config,
                            std::atomic<bool>& running,
                            SmsHandler* sms_handler) {
    pj_thread_desc thread_desc = {};
    pj_thread_t* pj_thread = nullptr;
    pj_status_t status = pj_thread_register(card_id_.c_str(), thread_desc, &pj_thread);
    if (status != PJ_SUCCESS) {
        LOG_ERROR("[%s] pj_thread_register failed (status=%d)", card_id_.c_str(), status);
        state_.store(CardState::FAILED, std::memory_order_release);
        return;
    }

    std::string sip_server_suffix = "@" + sip_config.server
                                    + ":" + std::to_string(sip_config.port)
                                    + sip_config.transport_param();

    if (sms_handler) {
        sms_handler->enable_sms_mode(*at_);
        LOG_INFO("[%s] SMS text mode enabled", card_id_.c_str());
    }

    if (own_number_.empty() && !bridge_config.sms.phone_number.empty()) {
        own_number_ = bridge_config.sms.phone_number;
        LOG_INFO("[%s] using configured phone_number: %s", card_id_.c_str(), own_number_.c_str());
    }

    LOG_INFO("[%s] listening for GSM calls%s", card_id_.c_str(),
             sms_handler ? " and SMS" : "");

    while (running.load(std::memory_order_relaxed) &&
           state_.load(std::memory_order_acquire) == CardState::ACTIVE) {
        auto urc = at_->poll_urc();
        if (!urc) continue;

        if (sms_handler && urc->find("+CMTI:") != std::string::npos) {
            sms_handler->handle_cmti(*at_, *urc, card_id_, own_number_);
            continue;
        }

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
                    auto clip_urc = at_->poll_urc();
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

            LOG_INFO("[%s] GSM RING%s%s", card_id_.c_str(),
                     caller.empty() ? "" : " from ",
                     caller.empty() ? "" : caller.c_str());

            metrics::gsm_call_incoming(card_id_, caller);

            if (!account.is_registered()) {
                LOG_WARN("[%s] SIP not registered, ignoring GSM call", card_id_.c_str());
                metrics::gsm_call_missed(card_id_);
                at_->hangup();
                continue;
            }

            if (at_->answer_call()) {
                metrics::gsm_call_answered(card_id_);
                std::string sip_user = bridge_config.sip_destination;
                if (sip_user.empty()) {
                    sip_user = caller;
                    if (!sip_user.empty() && sip_user[0] == '+') {
                        sip_user.erase(0, 1);
                    }
                }

                if (sip_user.empty()) {
                    LOG_WARN("[%s] no SIP destination and no caller ID, cannot route call",
                             card_id_.c_str());
                    at_->hangup();
                    continue;
                }

                std::string sip_dest_uri = "sip:" + sip_user + sip_server_suffix;

                LOG_INFO("[%s] GSM call answered, bridging to %s (caller: %s)",
                         card_id_.c_str(), sip_dest_uri.c_str(),
                         caller.empty() ? "unknown" : caller.c_str());
                handle_bridged_call(*at_, account, sip_dest_uri,
                                    bridge_config.sip_dial_timeout_sec,
                                    caller, running);
                LOG_INFO("[%s] idle, waiting for next GSM call", card_id_.c_str());
            } else {
                LOG_ERROR("[%s] failed to answer GSM call", card_id_.c_str());
                metrics::gsm_call_missed(card_id_);
            }
        }
    }

    LOG_INFO("[%s] call loop exiting", card_id_.c_str());
}
