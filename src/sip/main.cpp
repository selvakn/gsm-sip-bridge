#include "sip/sip_config.h"
#include "sip/echo_account.h"
#include "logger.h"

#include <pjsua2.hpp>
#include <atomic>
#include <csignal>
#include <cstring>
#include <getopt.h>
#include <string>
#include <thread>

static constexpr const char* VERSION = "0.1.0";
static constexpr const char* DEFAULT_CONFIG_PATH = "config.ini";

static std::atomic<bool> g_running{true};

static void signal_handler(int) {
    g_running.store(false, std::memory_order_relaxed);
}

struct CliArgs {
    std::string config_path = DEFAULT_CONFIG_PATH;
    bool verbose = false;
};

static int parse_args(int argc, char* argv[], CliArgs& args) {
    static struct option long_opts[] = {
        {"config",  required_argument, nullptr, 'c'},
        {"verbose", no_argument,       nullptr, 'v'},
        {"help",    no_argument,       nullptr, 'h'},
        {"version", no_argument,       nullptr, 'V'},
        {nullptr, 0, nullptr, 0}
    };

    int opt;
    while ((opt = getopt_long(argc, argv, "c:vh", long_opts, nullptr)) != -1) {
        switch (opt) {
            case 'c': args.config_path = optarg; break;
            case 'v': args.verbose = true; break;
            case 'h':
                std::printf(
                    "Usage: %s [OPTIONS]\n\n"
                    "SIP audio echo server. Registers with a SIP server and echoes\n"
                    "incoming call audio back to the caller.\n\n"
                    "Options:\n"
                    "  -c, --config PATH   Configuration file (default: %s)\n"
                    "  -v, --verbose       Enable verbose SIP logging\n"
                    "  -h, --help          Show this help\n"
                    "      --version       Show version\n",
                    argv[0], DEFAULT_CONFIG_PATH);
                return -1;
            case 'V':
                std::printf("sip-echo %s\n", VERSION);
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

    LOG_INFO("sip-echo v%s starting", VERSION);

    struct sigaction sa{};
    sa.sa_handler = signal_handler;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGINT, &sa, nullptr);
    sigaction(SIGTERM, &sa, nullptr);

    SipConfig config;
    auto result = SipConfig::load(args.config_path, config);
    if (!result.ok) {
        LOG_ERROR("config error: %s", result.error.c_str());
        return result.error.find("cannot read") != std::string::npos ? 1 : 2;
    }
    LOG_INFO("config loaded from %s", args.config_path.c_str());

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
        ep.libInit(ep_cfg);

        pj::TransportConfig tp_cfg;
        tp_cfg.port = 0;

        pjsip_transport_type_e tp_type = PJSIP_TRANSPORT_UDP;
        if (config.transport == "tcp") tp_type = PJSIP_TRANSPORT_TCP;
        else if (config.transport == "tls") tp_type = PJSIP_TRANSPORT_TLS;

        ep.transportCreate(tp_type, tp_cfg);
        ep.libStart();
        ep.audDevManager().setNullDev();

        LOG_INFO("PJSIP endpoint started (transport=%s)", config.transport.c_str());

    } catch (pj::Error& err) {
        LOG_ERROR("PJSIP init failed: %s", err.info().c_str());
        return 4;
    }

    EchoAccount account;
    try {
        pj::AccountConfig acc_cfg;
        acc_cfg.idUri = "\"" + config.display_name + "\" <" + config.sip_uri() + ">";
        acc_cfg.regConfig.registrarUri = config.registrar_uri();

        pj::AuthCredInfo cred("digest", "*", config.username, 0, config.password);
        acc_cfg.sipConfig.authCreds.push_back(cred);

        acc_cfg.regConfig.timeoutSec = 300;
        acc_cfg.regConfig.retryIntervalSec = 30;

        account.create(acc_cfg);
        LOG_INFO("registering as %s@%s:%u",
                 config.username.c_str(), config.server.c_str(), config.port);

    } catch (pj::Error& err) {
        LOG_ERROR("account creation failed: %s", err.info().c_str());
        return 4;
    }

    LOG_INFO("ready, waiting for incoming calls");

    while (g_running.load(std::memory_order_relaxed)) {
        std::this_thread::sleep_for(std::chrono::milliseconds(100));
    }

    LOG_INFO("shutting down, de-registering");

    try {
        account.shutdown();
        std::this_thread::sleep_for(std::chrono::milliseconds(500));
        ep.libDestroy();
    } catch (pj::Error& err) {
        LOG_ERROR("shutdown error: %s", err.info().c_str());
    }

    LOG_INFO("sip-echo stopped");
    return 0;
}
