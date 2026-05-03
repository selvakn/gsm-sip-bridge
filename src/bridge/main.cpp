#include "bridge/bridge_config.h"
#include "bridge/bridge_account.h"
#include "bridge/card_instance.h"
#include "bridge/card_pool.h"
#include "bridge/metrics.h"
#include "bridge/sms_handler.h"
#include "sip/sip_config.h"
#include "device_discovery.h"
#include "logger.h"

#include <pjsua2.hpp>

#include <atomic>
#include <chrono>
#include <csignal>
#include <cstdlib>
#include <cstring>
#include <getopt.h>
#include <string>
#include <thread>

static constexpr const char* VERSION = "4.0.0";
static constexpr const char* DEFAULT_CONFIG_PATH = "config.ini";

static std::atomic<bool> g_running{true};

static void signal_handler(int) {
    g_running.store(false, std::memory_order_relaxed);
}

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
                    "GSM to SIP audio bridge. Answers incoming GSM calls on one\n"
                    "or more EC20 modules and bridges audio to a SIP extension.\n\n"
                    "Options:\n"
                    "  -c, --config PATH   Configuration file (default: %s)\n"
                    "  -s, --serial PATH   Override serial port (single-card mode only)\n"
                    "  -a, --audio DEVICE  Override ALSA device (single-card mode only)\n"
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
             bridge_config.sip_destination.empty() ? "(PBX routing)" : bridge_config.sip_destination.c_str(),
             bridge_config.sip_dial_timeout_sec);

    uint16_t metrics_port = metrics::DEFAULT_METRICS_PORT;
    const char* metrics_port_env = std::getenv("METRICS_PORT");
    if (metrics_port_env) {
        metrics_port = static_cast<uint16_t>(std::atoi(metrics_port_env));
    }
    metrics::init(metrics_port);

    SmsHandler sms_handler(bridge_config.sms);
    if (!sms_handler.start()) {
        LOG_ERROR("SMS handler initialization failed");
        return 6;
    }

    SmsHandler* sms_ptr = bridge_config.sms.enabled ? &sms_handler : nullptr;

    if (std::system("systemctl is-active --quiet ModemManager 2>/dev/null") == 0) {
        LOG_WARN("ModemManager is running and may interfere with serial access");
    }

    // Discover and initialize cards
    CardPool pool;

    bool single_card_override = !args.serial_override.empty() && !args.audio_override.empty();
    if (single_card_override) {
        LOG_INFO("manual overrides: serial=%s, audio=%s (single-card mode)",
                 args.serial_override.c_str(), args.audio_override.c_str());

        DeviceInfo manual_device{args.serial_override, args.audio_override, "manual", "manual"};
        auto card = std::make_unique<CardInstance>(std::move(manual_device));
        if (!card->initialize(args.verbose)) {
            LOG_ERROR("manual card initialization failed: %s", card->fail_reason().c_str());
            return 1;
        }
        // For manual single-card, we manage the card directly outside CardPool
        // but we still need PJSIP running first — fall through to PJSIP init below
    } else {
        auto discover_result = pool.discover_and_initialize(args.verbose);
        if (!discover_result.ok) {
            LOG_ERROR("%s", discover_result.error.c_str());
            return 1;
        }
    }

    // Initialize PJSIP
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

    auto start_time = std::chrono::steady_clock::now();

    if (single_card_override) {
        DeviceInfo manual_device{args.serial_override, args.audio_override, "manual", "manual"};
        auto card = std::make_unique<CardInstance>(std::move(manual_device));
        if (!card->initialize(args.verbose)) {
            LOG_ERROR("manual card initialization failed: %s", card->fail_reason().c_str());
            return 1;
        }
        LOG_INFO("[%s] single-card mode, listening for GSM calls", card->card_id().c_str());
        card->start(account, bridge_config, sip_config, g_running, sms_ptr);

        while (g_running.load(std::memory_order_relaxed)) {
            auto elapsed = std::chrono::duration<double>(
                std::chrono::steady_clock::now() - start_time).count();
            metrics::uptime_update(elapsed);
            std::this_thread::sleep_for(std::chrono::milliseconds(200));
        }

        card->stop();
    } else {
        pool.print_summary();
        pool.start_all(account, bridge_config, sip_config, g_running, sms_ptr);
        pool.start_retry_thread(account, bridge_config, sip_config, g_running, sms_ptr);

        while (g_running.load(std::memory_order_relaxed)) {
            auto elapsed = std::chrono::duration<double>(
                std::chrono::steady_clock::now() - start_time).count();
            metrics::uptime_update(elapsed);
            std::this_thread::sleep_for(std::chrono::milliseconds(200));
        }

        LOG_INFO("shutting down");
        pool.stop_all();
    }

    try {
        account.hangup_all_calls();
        account.shutdown();
        std::this_thread::sleep_for(std::chrono::milliseconds(500));
        ep.libDestroy();
    } catch (pj::Error& err) {
        LOG_ERROR("shutdown: %s", err.info().c_str());
    }

    sms_handler.stop();
    metrics::shutdown();
    LOG_INFO("gsm-sip-bridge stopped");
    return 0;
}
