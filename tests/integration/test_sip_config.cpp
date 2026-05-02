#include <gtest/gtest.h>
#include "sip/sip_config.h"
#include <fstream>
#include <cstdio>

class SipConfigTest : public ::testing::Test {
protected:
    std::string tmp_path_;

    void SetUp() override {
        tmp_path_ = "/tmp/test_sip_config_" +
                     std::to_string(::getpid()) + ".ini";
    }

    void TearDown() override {
        std::remove(tmp_path_.c_str());
    }

    void write_ini(const std::string& content) {
        std::ofstream f(tmp_path_);
        f << content;
    }
};

TEST_F(SipConfigTest, load_valid_config_succeeds) {
    // Arrange
    write_ini(
        "[sip]\n"
        "server = pbx.example.com\n"
        "port = 5080\n"
        "username = testuser\n"
        "password = secret\n"
        "display_name = Test Echo\n"
        "transport = tcp\n"
    );

    // Act
    SipConfig cfg;
    auto result = SipConfig::load(tmp_path_, cfg);

    // Assert
    ASSERT_TRUE(result.ok);
    EXPECT_EQ(cfg.server, "pbx.example.com");
    EXPECT_EQ(cfg.port, 5080);
    EXPECT_EQ(cfg.username, "testuser");
    EXPECT_EQ(cfg.password, "secret");
    EXPECT_EQ(cfg.display_name, "Test Echo");
    EXPECT_EQ(cfg.transport, "tcp");
}

TEST_F(SipConfigTest, load_minimal_config_uses_defaults) {
    // Arrange
    write_ini(
        "[sip]\n"
        "server = sip.local\n"
        "username = echo\n"
        "password = pass\n"
    );

    // Act
    SipConfig cfg;
    auto result = SipConfig::load(tmp_path_, cfg);

    // Assert
    ASSERT_TRUE(result.ok);
    EXPECT_EQ(cfg.port, 5060);
    EXPECT_EQ(cfg.display_name, "echo");
    EXPECT_EQ(cfg.transport, "udp");
}

TEST_F(SipConfigTest, load_missing_file_fails) {
    // Arrange / Act
    SipConfig cfg;
    auto result = SipConfig::load("/tmp/nonexistent_config.ini", cfg);

    // Assert
    EXPECT_FALSE(result.ok);
    EXPECT_NE(result.error.find("cannot read"), std::string::npos);
}

TEST_F(SipConfigTest, load_missing_sip_section_fails) {
    // Arrange
    write_ini("[other]\nkey = val\n");

    // Act
    SipConfig cfg;
    auto result = SipConfig::load(tmp_path_, cfg);

    // Assert
    EXPECT_FALSE(result.ok);
    EXPECT_NE(result.error.find("[sip]"), std::string::npos);
}

TEST_F(SipConfigTest, load_missing_server_fails) {
    // Arrange
    write_ini("[sip]\nusername = u\npassword = p\n");

    // Act
    SipConfig cfg;
    auto result = SipConfig::load(tmp_path_, cfg);

    // Assert
    EXPECT_FALSE(result.ok);
    EXPECT_NE(result.error.find("server"), std::string::npos);
}

TEST_F(SipConfigTest, load_missing_username_fails) {
    // Arrange
    write_ini("[sip]\nserver = s\npassword = p\n");

    // Act
    SipConfig cfg;
    auto result = SipConfig::load(tmp_path_, cfg);

    // Assert
    EXPECT_FALSE(result.ok);
    EXPECT_NE(result.error.find("username"), std::string::npos);
}

TEST_F(SipConfigTest, load_missing_password_fails) {
    // Arrange
    write_ini("[sip]\nserver = s\nusername = u\n");

    // Act
    SipConfig cfg;
    auto result = SipConfig::load(tmp_path_, cfg);

    // Assert
    EXPECT_FALSE(result.ok);
    EXPECT_NE(result.error.find("password"), std::string::npos);
}

TEST_F(SipConfigTest, load_invalid_port_fails) {
    // Arrange
    write_ini("[sip]\nserver = s\nusername = u\npassword = p\nport = 99999\n");

    // Act
    SipConfig cfg;
    auto result = SipConfig::load(tmp_path_, cfg);

    // Assert
    EXPECT_FALSE(result.ok);
    EXPECT_NE(result.error.find("port"), std::string::npos);
}

TEST_F(SipConfigTest, load_invalid_transport_fails) {
    // Arrange
    write_ini("[sip]\nserver = s\nusername = u\npassword = p\ntransport = sctp\n");

    // Act
    SipConfig cfg;
    auto result = SipConfig::load(tmp_path_, cfg);

    // Assert
    EXPECT_FALSE(result.ok);
    EXPECT_NE(result.error.find("transport"), std::string::npos);
}

TEST_F(SipConfigTest, sip_uri_format) {
    // Arrange
    SipConfig cfg;
    cfg.server = "pbx.local";
    cfg.username = "echo";

    // Act / Assert
    EXPECT_EQ(cfg.sip_uri(), "sip:echo@pbx.local");
}

TEST_F(SipConfigTest, registrar_uri_format) {
    // Arrange
    SipConfig cfg;
    cfg.server = "pbx.local";
    cfg.port = 5080;

    // Act / Assert
    EXPECT_EQ(cfg.registrar_uri(), "sip:pbx.local:5080");
}
