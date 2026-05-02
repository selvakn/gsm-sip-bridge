#include "sip/sip_config.h"
#include <mini/ini.h>

static constexpr uint16_t MIN_PORT = 1;
static constexpr uint16_t MAX_PORT = 65535;

SipConfig::LoadResult SipConfig::load(const std::string& path, SipConfig& out) {
    mINI::INIFile file(path);
    mINI::INIStructure ini;

    if (!file.read(ini)) {
        return {false, "cannot read config file: " + path};
    }

    if (!ini.has("sip")) {
        return {false, "missing [sip] section"};
    }

    auto& sip = ini["sip"];

    if (!sip.has("server") || sip["server"].empty()) {
        return {false, "missing required field 'server'"};
    }
    out.server = sip["server"];

    if (!sip.has("username") || sip["username"].empty()) {
        return {false, "missing required field 'username'"};
    }
    out.username = sip["username"];

    if (!sip.has("password") || sip["password"].empty()) {
        return {false, "missing required field 'password'"};
    }
    out.password = sip["password"];

    if (sip.has("port") && !sip["port"].empty()) {
        int port_val = 0;
        try {
            port_val = std::stoi(sip["port"]);
        } catch (...) {
            return {false, "invalid port value: " + sip["port"]};
        }
        if (port_val < MIN_PORT || port_val > MAX_PORT) {
            return {false, "port out of range (1-65535): " + sip["port"]};
        }
        out.port = static_cast<uint16_t>(port_val);
    }

    if (sip.has("display_name") && !sip["display_name"].empty()) {
        out.display_name = sip["display_name"];
    } else {
        out.display_name = out.username;
    }

    if (sip.has("transport") && !sip["transport"].empty()) {
        std::string t = sip["transport"];
        if (t != "udp" && t != "tcp" && t != "tls") {
            return {false, "invalid transport (must be udp, tcp, or tls): " + t};
        }
        out.transport = t;
    }

    return {true, ""};
}

std::string SipConfig::sip_uri() const {
    return "sip:" + username + "@" + server;
}

std::string SipConfig::registrar_uri() const {
    return "sip:" + server + ":" + std::to_string(port);
}
