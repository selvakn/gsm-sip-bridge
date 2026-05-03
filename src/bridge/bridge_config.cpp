#include "bridge/bridge_config.h"
#include <mini/ini.h>
#include <algorithm>

static constexpr uint16_t MIN_TIMEOUT_SEC = 5;
static constexpr uint16_t MAX_TIMEOUT_SEC = 120;
static constexpr size_t MAX_DESTINATION_LEN = 32;

static bool is_valid_destination(const std::string& dest) {
    if (dest.empty() || dest.size() > MAX_DESTINATION_LEN) return false;
    return std::all_of(dest.begin(), dest.end(), [](char c) {
        return std::isalnum(c) || c == '*' || c == '#' || c == '+';
    });
}

BridgeConfig::LoadResult BridgeConfig::load(const std::string& path, BridgeConfig& out) {
    mINI::INIFile file(path);
    mINI::INIStructure ini;

    if (!file.read(ini)) {
        return {false, "cannot read config file: " + path};
    }

    if (ini.has("bridge")) {
        auto& bridge = ini["bridge"];

        if (bridge.has("sip_destination") && !bridge["sip_destination"].empty()) {
            std::string dest = bridge["sip_destination"];
            if (!is_valid_destination(dest)) {
                return {false, "invalid sip_destination (alphanumeric/*/#+, max 32 chars): " + dest};
            }
            out.sip_destination = dest;
        } else {
            out.sip_destination.clear();
        }

        if (bridge.has("sip_dial_timeout_sec") && !bridge["sip_dial_timeout_sec"].empty()) {
            int val = 0;
            try {
                val = std::stoi(bridge["sip_dial_timeout_sec"]);
            } catch (...) {
                return {false, "invalid sip_dial_timeout_sec: " + bridge["sip_dial_timeout_sec"]};
            }
            if (val < MIN_TIMEOUT_SEC || val > MAX_TIMEOUT_SEC) {
                return {false, "sip_dial_timeout_sec out of range (5-120): " + std::to_string(val)};
            }
            out.sip_dial_timeout_sec = static_cast<uint16_t>(val);
        }
    }

    if (ini.has("sms")) {
        auto& sms = ini["sms"];

        if (sms.has("enabled") && !sms["enabled"].empty()) {
            std::string val = sms["enabled"];
            if (val == "false" || val == "0" || val == "no") {
                out.sms.enabled = false;
            }
        }

        if (sms.has("discord_webhook_url") && !sms["discord_webhook_url"].empty()) {
            out.sms.discord_webhook_url = sms["discord_webhook_url"];
        }

        if (sms.has("db_path") && !sms["db_path"].empty()) {
            out.sms.db_path = sms["db_path"];
        }

        if (sms.has("phone_number") && !sms["phone_number"].empty()) {
            out.sms.phone_number = sms["phone_number"];
        }
    }

    return {true, ""};
}
