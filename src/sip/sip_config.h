#pragma once

#include <cstdint>
#include <string>

struct SipConfig {
    std::string server;
    uint16_t port = 5060;
    std::string username;
    std::string password;
    std::string display_name;
    std::string transport = "udp";

    struct LoadResult {
        bool ok = false;
        std::string error;
    };

    static LoadResult load(const std::string& path, SipConfig& out);

    std::string sip_uri() const;
    std::string registrar_uri() const;
};
