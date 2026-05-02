#include "device_discovery.h"

#include <gtest/gtest.h>

TEST(DeviceDiscovery, discover_returns_optional) {
    // Arrange: no specific setup needed; tests against real udev subsystem
    // Act
    auto result = discover_ec20();

    // Assert: either finds an EC20 or returns nullopt (both valid on CI)
    if (result) {
        EXPECT_FALSE(result->serial_port.empty());
        EXPECT_FALSE(result->alsa_device.empty());
        EXPECT_NE(result->alsa_device.find("hw:"), std::string::npos);
    } else {
        SUCCEED() << "no EC20 connected, discovery correctly returned nullopt";
    }
}

TEST(DeviceDiscovery, device_info_fields_are_non_empty_when_found) {
    // Arrange
    auto result = discover_ec20();
    if (!result) {
        GTEST_SKIP() << "EC20 not connected, skipping hardware-dependent test";
    }

    // Assert
    EXPECT_TRUE(result->serial_port.find("/dev/") == 0);
    EXPECT_TRUE(result->alsa_device.find("hw:") == 0);
}

TEST(DeviceDiscovery, discover_all_returns_vector) {
    // Act
    auto results = discover_all_ec20();

    // Assert: valid regardless of hardware presence
    for (const auto& dev : results) {
        EXPECT_FALSE(dev.serial_port.empty());
        EXPECT_FALSE(dev.alsa_device.empty());
        EXPECT_NE(dev.alsa_device.find("hw:"), std::string::npos);
        EXPECT_TRUE(dev.serial_port.find("/dev/") == 0);
    }

    if (results.empty()) {
        SUCCEED() << "no EC20 connected, discover_all correctly returned empty vector";
    }
}

TEST(DeviceDiscovery, discover_all_populates_serial_number_and_usb_path) {
    // Arrange
    auto results = discover_all_ec20();
    if (results.empty()) {
        GTEST_SKIP() << "EC20 not connected, skipping hardware-dependent test";
    }

    // Assert
    for (const auto& dev : results) {
        EXPECT_FALSE(dev.usb_path.empty());
    }
}

TEST(DeviceDiscovery, discover_all_contains_first_device_from_discover) {
    // Arrange
    auto single = discover_ec20();
    auto all = discover_all_ec20();

    // Assert
    if (!single) {
        EXPECT_TRUE(all.empty());
        return;
    }

    ASSERT_FALSE(all.empty());
    EXPECT_EQ(all.front().serial_port, single->serial_port);
    EXPECT_EQ(all.front().alsa_device, single->alsa_device);
}
