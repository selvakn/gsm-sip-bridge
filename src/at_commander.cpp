#include "at_commander.h"
#include "logger.h"

#include <chrono>
#include <thread>

const char* call_state_str(CallState state) {
    switch (state) {
        case CallState::IDLE:     return "IDLE";
        case CallState::RINGING:  return "RINGING";
        case CallState::ANSWERED: return "ANSWERED";
        case CallState::ECHOING:  return "ECHOING";
        case CallState::ENDED:    return "ENDED";
    }
    return "UNKNOWN";
}

AtCommander::AtCommander(SerialPort& port) : port_(port) {}

bool AtCommander::send(const std::string& command) {
    if (verbose_) {
        LOG_INFO("[AT] >>> %s", command.c_str());
    }
    return port_.write_line(command);
}

std::optional<std::string> AtCommander::read_response(int timeout_ms) {
    auto deadline = std::chrono::steady_clock::now() +
                    std::chrono::milliseconds(timeout_ms);

    while (std::chrono::steady_clock::now() < deadline) {
        auto line = port_.read_line();
        if (line) {
            if (verbose_) {
                LOG_INFO("[AT] <<< %s", line->c_str());
            }
            return line;
        }
    }
    return std::nullopt;
}

bool AtCommander::send_and_expect_ok(const std::string& command, int timeout_ms) {
    if (!send(command)) return false;

    auto deadline = std::chrono::steady_clock::now() +
                    std::chrono::milliseconds(timeout_ms);

    while (std::chrono::steady_clock::now() < deadline) {
        auto remaining = std::chrono::duration_cast<std::chrono::milliseconds>(
            deadline - std::chrono::steady_clock::now()).count();
        if (remaining <= 0) break;

        auto line = read_response(static_cast<int>(remaining));
        if (!line) continue;

        // Skip echo of the command itself (modem echo may still be on)
        if (line->find(command) != std::string::npos) continue;
        // Skip empty or whitespace-only lines
        if (line->find_first_not_of(" \t") == std::string::npos) continue;

        if (*line == "OK") return true;
        if (line->find("ERROR") != std::string::npos) {
            LOG_ERROR("AT command '%s' returned: %s", command.c_str(), line->c_str());
            return false;
        }
    }

    LOG_ERROR("AT command '%s' timed out", command.c_str());
    return false;
}

bool AtCommander::answer_call() {
    return send_and_expect_ok("ATA", 5000);
}

bool AtCommander::hangup() {
    return send_and_expect_ok("AT+CHUP", 3000);
}

bool AtCommander::query_network_registration() {
    if (!send("AT+COPS?")) return false;

    auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(3000);
    bool registered = false;

    while (std::chrono::steady_clock::now() < deadline) {
        auto remaining = std::chrono::duration_cast<std::chrono::milliseconds>(
            deadline - std::chrono::steady_clock::now()).count();
        if (remaining <= 0) break;

        auto line = read_response(static_cast<int>(remaining));
        if (!line) continue;

        if (line->find("+COPS:") != std::string::npos && line->size() > 7) {
            registered = true;
        }
        if (*line == "OK") break;
    }
    return registered;
}

std::optional<std::string> AtCommander::poll_urc() {
    auto line = port_.read_line();
    if (line && verbose_) {
        LOG_INFO("[AT] <<< %s", line->c_str());
    }
    return line;
}
