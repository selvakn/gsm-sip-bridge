#pragma once

#include <cstdint>
#include <string>

struct SmsConfig {
    bool enabled = true;
    std::string discord_webhook_url;
    std::string db_path = "/var/lib/gsm-sip-bridge/sms.db";
    std::string phone_number;
};

struct BridgeConfig {
    std::string sip_destination;
    uint16_t sip_dial_timeout_sec = 30;
    SmsConfig sms;

    struct LoadResult {
        bool ok = false;
        std::string error;
    };

    static LoadResult load(const std::string& path, BridgeConfig& out);
};
