#include "at_commander.h"
#include "audio_loop.h"
#include "device_discovery.h"
#include "logger.h"
#include "serial_port.h"

#include <atomic>
#include <csignal>
#include <cstring>
#include <getopt.h>
#include <string>
#include <thread>

static constexpr const char* VERSION = "0.1.0";
static constexpr int RECONNECT_INTERVAL_SEC = 5;

static std::atomic<bool> g_running{true};

static void signal_handler(int) {
    g_running.store(false, std::memory_order_relaxed);
}

static void print_usage(const char* prog) {
    std::printf(
        "Usage: %s [OPTIONS]\n\n"
        "Auto-answer incoming GSM calls and echo audio back to the caller.\n\n"
        "Options:\n"
        "  -s, --serial PATH   Override serial port (default: auto-detect)\n"
        "  -a, --audio DEVICE  Override ALSA device (default: auto-detect)\n"
        "  -v, --verbose       Enable verbose AT command logging\n"
        "  -h, --help          Show this help and exit\n"
        "      --version       Show version and exit\n",
        prog);
}

struct CliArgs {
    std::string serial_override;
    std::string audio_override;
    bool verbose = false;
};

static int parse_args(int argc, char* argv[], CliArgs& args) {
    static struct option long_opts[] = {
        {"serial",  required_argument, nullptr, 's'},
        {"audio",   required_argument, nullptr, 'a'},
        {"verbose", no_argument,       nullptr, 'v'},
        {"help",    no_argument,       nullptr, 'h'},
        {"version", no_argument,       nullptr, 'V'},
        {nullptr, 0, nullptr, 0}
    };

    int opt;
    while ((opt = getopt_long(argc, argv, "s:a:vh", long_opts, nullptr)) != -1) {
        switch (opt) {
            case 's': args.serial_override = optarg; break;
            case 'a': args.audio_override = optarg;  break;
            case 'v': args.verbose = true;           break;
            case 'h': print_usage(argv[0]); return -1;
            case 'V':
                std::printf("audio-echo %s\n", VERSION);
                return -1;
            default:
                print_usage(argv[0]);
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
        LOG_INFO("using manual overrides: serial=%s, audio=%s",
                 info.serial_port.c_str(), info.alsa_device.c_str());
        return info;
    }

    auto detected = discover_ec20();
    if (!detected) {
        LOG_ERROR("no EC20 module found (USB %04X:%04X)", EC20_VENDOR_ID, EC20_PRODUCT_ID);
        return {};
    }

    info = *detected;
    if (!args.serial_override.empty()) {
        info.serial_port = args.serial_override;
        LOG_INFO("serial port overridden to %s", info.serial_port.c_str());
    }
    if (!args.audio_override.empty()) {
        info.alsa_device = args.audio_override;
        LOG_INFO("audio device overridden to %s", info.alsa_device.c_str());
    }
    return info;
}

static void handle_echo_call(AtCommander& at, const std::string& alsa_device) {
    LOG_INFO("call answered, echo active");

    AudioLoop audio;
    if (!audio.open(alsa_device)) {
        LOG_ERROR("failed to open audio, hanging up");
        at.hangup();
        return;
    }

    std::atomic<bool> echoing{true};
    std::thread audio_thread([&audio, &echoing]() {
        audio.run(echoing);
    });

    while (g_running.load(std::memory_order_relaxed) &&
           echoing.load(std::memory_order_relaxed)) {
        auto urc = at.poll_urc();
        if (!urc) continue;

        if (urc->find("NO CARRIER") != std::string::npos ||
            urc->find("BUSY") != std::string::npos ||
            urc->find("NO ANSWER") != std::string::npos) {
            LOG_INFO("call ended (remote hangup)");
            echoing.store(false, std::memory_order_relaxed);
        }

        if (urc->find("RING") != std::string::npos) {
            LOG_INFO("rejecting second call while echo active");
            at.hangup();
        }
    }

    echoing.store(false, std::memory_order_relaxed);
    audio.close();
    if (audio_thread.joinable()) audio_thread.join();
}

static int run_event_loop(AtCommander& at, const std::string& alsa_device) {
    CallState state = CallState::IDLE;
    LOG_INFO("ready, waiting for incoming calls");

    while (g_running.load(std::memory_order_relaxed)) {
        auto urc = at.poll_urc();
        if (!urc) continue;

        if (state == CallState::IDLE && urc->find("RING") != std::string::npos) {
            state = CallState::RINGING;
            std::string caller;
            if (urc->find("+CLIP:") != std::string::npos) {
                auto start = urc->find('"');
                auto end = urc->find('"', start + 1);
                if (start != std::string::npos && end != std::string::npos) {
                    caller = urc->substr(start + 1, end - start - 1);
                }
            }
            LOG_INFO("RING%s%s", caller.empty() ? "" : " from ",
                     caller.empty() ? "" : caller.c_str());

            if (at.answer_call()) {
                state = CallState::ECHOING;
                handle_echo_call(at, alsa_device);
            } else {
                LOG_ERROR("failed to answer call");
            }
            state = CallState::IDLE;
            LOG_INFO("idle, waiting for next call");
        }
    }

    return 0;
}

int main(int argc, char* argv[]) {
    CliArgs args;
    int parse_result = parse_args(argc, argv, args);
    if (parse_result != 0) return parse_result < 0 ? 0 : parse_result;

    LOG_INFO("audio-echo v%s starting", VERSION);

    struct sigaction sa{};
    sa.sa_handler = signal_handler;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGINT, &sa, nullptr);
    sigaction(SIGTERM, &sa, nullptr);

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

    // Drain any stale data in the serial buffer from prior sessions
    for (int i = 0; i < 20; ++i) {
        if (!serial.read_line()) break;
    }

    // Disable modem echo; retry once since the first attempt may be
    // garbled by the modem's own echo of the command
    if (!at.send_and_expect_ok("ATE0", 2000)) {
        at.send_and_expect_ok("ATE0", 2000);
    }

    at.send_and_expect_ok("AT+CLIP=1", 2000);

    // Route voice call audio through USB Audio Class (UAC) interface.
    // Without this, audio goes to the analog PCM pins, not USB.
    if (!at.send_and_expect_ok("AT+QPCMV=1,2", 2000)) {
        LOG_WARN("AT+QPCMV=1,2 failed, trying AT+QPCMV=1,0");
        at.send_and_expect_ok("AT+QPCMV=1,0", 2000);
    }

    if (!at.query_network_registration()) {
        LOG_ERROR("SIM not registered on network");
        return 4;
    }
    LOG_INFO("network registration confirmed");

    int result = run_event_loop(at, device.alsa_device);

    LOG_INFO("shutting down");
    at.hangup();
    serial.close();

    return result;
}
