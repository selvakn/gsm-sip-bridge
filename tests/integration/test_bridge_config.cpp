#include <gtest/gtest.h>
#include "bridge/bridge_config.h"
#include <cstdio>
#include <fstream>
#include <string>

static std::string create_temp_file(const std::string& content) {
    char path[] = "/tmp/bridge_config_XXXXXX";
    int fd = mkstemp(path);
    if (fd < 0) return {};
    write(fd, content.c_str(), content.size());
    close(fd);
    return path;
}

TEST(BridgeConfig, valid_full_config) {
    // Arrange
    auto path = create_temp_file(
        "[sip]\nserver=pbx\nusername=user\npassword=pass\n"
        "[bridge]\nsip_destination = 100\nsip_dial_timeout_sec = 60\n");

    // Act
    BridgeConfig config;
    auto result = BridgeConfig::load(path, config);

    // Assert
    EXPECT_TRUE(result.ok);
    EXPECT_EQ(config.sip_destination, "100");
    EXPECT_EQ(config.sip_dial_timeout_sec, 60);
    std::remove(path.c_str());
}

TEST(BridgeConfig, missing_bridge_section_leaves_destination_empty) {
    // Arrange
    auto path = create_temp_file("[sip]\nserver=pbx\nusername=u\npassword=p\n");

    // Act
    BridgeConfig config;
    auto result = BridgeConfig::load(path, config);

    // Assert
    EXPECT_TRUE(result.ok);
    EXPECT_TRUE(config.sip_destination.empty());
    EXPECT_EQ(config.sip_dial_timeout_sec, 30);
    std::remove(path.c_str());
}

TEST(BridgeConfig, empty_destination_enables_pbx_routing) {
    // Arrange
    auto path = create_temp_file("[bridge]\nsip_destination = \n");

    // Act
    BridgeConfig config;
    auto result = BridgeConfig::load(path, config);

    // Assert
    EXPECT_TRUE(result.ok);
    EXPECT_TRUE(config.sip_destination.empty());
    std::remove(path.c_str());
}

TEST(BridgeConfig, omitted_destination_enables_pbx_routing) {
    // Arrange
    auto path = create_temp_file("[bridge]\nsip_dial_timeout_sec = 15\n");

    // Act
    BridgeConfig config;
    auto result = BridgeConfig::load(path, config);

    // Assert
    EXPECT_TRUE(result.ok);
    EXPECT_TRUE(config.sip_destination.empty());
    EXPECT_EQ(config.sip_dial_timeout_sec, 15);
    std::remove(path.c_str());
}

TEST(BridgeConfig, timeout_too_low_returns_error) {
    // Arrange
    auto path = create_temp_file("[bridge]\nsip_dial_timeout_sec = 2\n");

    // Act
    BridgeConfig config;
    auto result = BridgeConfig::load(path, config);

    // Assert
    EXPECT_FALSE(result.ok);
    EXPECT_NE(result.error.find("out of range"), std::string::npos);
    std::remove(path.c_str());
}

TEST(BridgeConfig, timeout_too_high_returns_error) {
    // Arrange
    auto path = create_temp_file("[bridge]\nsip_dial_timeout_sec = 999\n");

    // Act
    BridgeConfig config;
    auto result = BridgeConfig::load(path, config);

    // Assert
    EXPECT_FALSE(result.ok);
    EXPECT_NE(result.error.find("out of range"), std::string::npos);
    std::remove(path.c_str());
}

TEST(BridgeConfig, timeout_boundary_min_valid) {
    // Arrange
    auto path = create_temp_file("[bridge]\nsip_dial_timeout_sec = 5\n");

    // Act
    BridgeConfig config;
    auto result = BridgeConfig::load(path, config);

    // Assert
    EXPECT_TRUE(result.ok);
    EXPECT_EQ(config.sip_dial_timeout_sec, 5);
    std::remove(path.c_str());
}

TEST(BridgeConfig, timeout_boundary_max_valid) {
    // Arrange
    auto path = create_temp_file("[bridge]\nsip_dial_timeout_sec = 120\n");

    // Act
    BridgeConfig config;
    auto result = BridgeConfig::load(path, config);

    // Assert
    EXPECT_TRUE(result.ok);
    EXPECT_EQ(config.sip_dial_timeout_sec, 120);
    std::remove(path.c_str());
}

TEST(BridgeConfig, invalid_destination_chars) {
    // Arrange
    auto path = create_temp_file("[bridge]\nsip_destination = abc;DROP TABLE\n");

    // Act
    BridgeConfig config;
    auto result = BridgeConfig::load(path, config);

    // Assert
    EXPECT_FALSE(result.ok);
    EXPECT_NE(result.error.find("invalid sip_destination"), std::string::npos);
    std::remove(path.c_str());
}

TEST(BridgeConfig, missing_file_returns_error) {
    // Arrange / Act
    BridgeConfig config;
    auto result = BridgeConfig::load("/nonexistent/config.ini", config);

    // Assert
    EXPECT_FALSE(result.ok);
    EXPECT_NE(result.error.find("cannot read"), std::string::npos);
}

TEST(BridgeConfig, destination_with_special_chars) {
    // Arrange
    auto path = create_temp_file("[bridge]\nsip_destination = +1234*#\n");

    // Act
    BridgeConfig config;
    auto result = BridgeConfig::load(path, config);

    // Assert
    EXPECT_TRUE(result.ok);
    EXPECT_EQ(config.sip_destination, "+1234*#");
    std::remove(path.c_str());
}

TEST(BridgeConfig, sms_section_defaults) {
    // Arrange
    auto path = create_temp_file("[sip]\nserver=pbx\nusername=u\npassword=p\n");

    // Act
    BridgeConfig config;
    auto result = BridgeConfig::load(path, config);

    // Assert
    EXPECT_TRUE(result.ok);
    EXPECT_TRUE(config.sms.enabled);
    EXPECT_TRUE(config.sms.discord_webhook_url.empty());
    EXPECT_EQ(config.sms.db_path, "/var/lib/gsm-sip-bridge/sms.db");
    std::remove(path.c_str());
}

TEST(BridgeConfig, sms_section_full_config) {
    // Arrange
    auto path = create_temp_file(
        "[sms]\n"
        "enabled = true\n"
        "discord_webhook_url = https://discord.com/api/webhooks/123/abc\n"
        "db_path = /tmp/test.db\n");

    // Act
    BridgeConfig config;
    auto result = BridgeConfig::load(path, config);

    // Assert
    EXPECT_TRUE(result.ok);
    EXPECT_TRUE(config.sms.enabled);
    EXPECT_EQ(config.sms.discord_webhook_url, "https://discord.com/api/webhooks/123/abc");
    EXPECT_EQ(config.sms.db_path, "/tmp/test.db");
    std::remove(path.c_str());
}

TEST(BridgeConfig, sms_section_disabled) {
    // Arrange
    auto path = create_temp_file("[sms]\nenabled = false\n");

    // Act
    BridgeConfig config;
    auto result = BridgeConfig::load(path, config);

    // Assert
    EXPECT_TRUE(result.ok);
    EXPECT_FALSE(config.sms.enabled);
    std::remove(path.c_str());
}

TEST(BridgeConfig, sms_section_disabled_with_zero) {
    // Arrange
    auto path = create_temp_file("[sms]\nenabled = 0\n");

    // Act
    BridgeConfig config;
    auto result = BridgeConfig::load(path, config);

    // Assert
    EXPECT_TRUE(result.ok);
    EXPECT_FALSE(config.sms.enabled);
    std::remove(path.c_str());
}

TEST(BridgeConfig, sms_phone_number_parsed) {
    // Arrange
    auto path = create_temp_file("[sms]\nphone_number = +9876543210\n");

    // Act
    BridgeConfig config;
    auto result = BridgeConfig::load(path, config);

    // Assert
    EXPECT_TRUE(result.ok);
    EXPECT_EQ(config.sms.phone_number, "+9876543210");
    std::remove(path.c_str());
}

TEST(BridgeConfig, sms_phone_number_default_empty) {
    // Arrange
    auto path = create_temp_file("[sms]\nenabled = true\n");

    // Act
    BridgeConfig config;
    auto result = BridgeConfig::load(path, config);

    // Assert
    EXPECT_TRUE(result.ok);
    EXPECT_TRUE(config.sms.phone_number.empty());
    std::remove(path.c_str());
}
