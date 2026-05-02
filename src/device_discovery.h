#pragma once

#include <cstdint>
#include <optional>
#include <string>
#include <vector>

struct DeviceInfo {
    std::string serial_port;
    std::string alsa_device;
    std::string serial_number;
    std::string usb_path;
};

constexpr uint16_t EC20_VENDOR_ID  = 0x2C7C;
constexpr uint16_t EC20_PRODUCT_ID = 0x0125;

std::vector<DeviceInfo> discover_all_ec20();
std::optional<DeviceInfo> discover_ec20();
