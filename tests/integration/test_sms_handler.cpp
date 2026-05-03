#include <gtest/gtest.h>
#include "bridge/sms_handler.h"

class SmsParseTest : public ::testing::Test {};

TEST_F(SmsParseTest, parse_cmti_valid_sm) {
    int index = -1;
    ASSERT_TRUE(SmsHandler::parse_cmti("+CMTI: \"SM\",3", index));
    EXPECT_EQ(index, 3);
}

TEST_F(SmsParseTest, parse_cmti_valid_me) {
    int index = -1;
    ASSERT_TRUE(SmsHandler::parse_cmti("+CMTI: \"ME\",15", index));
    EXPECT_EQ(index, 15);
}

TEST_F(SmsParseTest, parse_cmti_zero_index) {
    int index = -1;
    ASSERT_TRUE(SmsHandler::parse_cmti("+CMTI: \"SM\",0", index));
    EXPECT_EQ(index, 0);
}

TEST_F(SmsParseTest, parse_cmti_no_comma) {
    int index = -1;
    EXPECT_FALSE(SmsHandler::parse_cmti("+CMTI: \"SM\"", index));
}

TEST_F(SmsParseTest, parse_cmti_not_cmti) {
    int index = -1;
    EXPECT_FALSE(SmsHandler::parse_cmti("+CMT: something", index));
}

TEST_F(SmsParseTest, parse_cmgr_valid) {
    std::string header = R"(+CMGR: "REC UNREAD","+1234567890","","2026/05/03,14:30:00+20")";
    std::string body = "Hello from GSM";

    SmsMessage msg;
    ASSERT_TRUE(SmsHandler::parse_cmgr(header, body, msg));
    EXPECT_EQ(msg.sender, "+1234567890");
    EXPECT_EQ(msg.body, "Hello from GSM");
    EXPECT_EQ(msg.timestamp, "2026/05/03,14:30:00+20");
}

TEST_F(SmsParseTest, parse_cmgr_read_status) {
    std::string header = R"(+CMGR: "REC READ","+9876543210","","2026/01/15,08:00:00+00")";
    std::string body = "Already read";

    SmsMessage msg;
    ASSERT_TRUE(SmsHandler::parse_cmgr(header, body, msg));
    EXPECT_EQ(msg.sender, "+9876543210");
    EXPECT_EQ(msg.body, "Already read");
}

TEST_F(SmsParseTest, parse_cmgr_empty_body) {
    std::string header = R"(+CMGR: "REC UNREAD","+1234567890","","2026/05/03,14:30:00+20")";
    std::string body = "";

    SmsMessage msg;
    ASSERT_TRUE(SmsHandler::parse_cmgr(header, body, msg));
    EXPECT_EQ(msg.sender, "+1234567890");
    EXPECT_EQ(msg.body, "");
}

TEST_F(SmsParseTest, parse_cmgr_body_with_trailing_newlines) {
    std::string header = R"(+CMGR: "REC UNREAD","+1234567890","","2026/05/03,14:30:00+20")";
    std::string body = "Hello\r\n";

    SmsMessage msg;
    ASSERT_TRUE(SmsHandler::parse_cmgr(header, body, msg));
    EXPECT_EQ(msg.body, "Hello");
}

TEST_F(SmsParseTest, parse_cmgr_no_cmgr_prefix) {
    SmsMessage msg;
    EXPECT_FALSE(SmsHandler::parse_cmgr("SOME OTHER LINE", "body", msg));
}

class DiscordPayloadTest : public ::testing::Test {};

TEST_F(DiscordPayloadTest, build_payload_basic) {
    SmsMessage msg;
    msg.sender = "+1234567890";
    msg.body = "Hello world";
    msg.timestamp = "2026-05-03T14:30:00Z";
    msg.module_id = "ec20-abc123";
    msg.receiver = "+9876543210";

    std::string payload = SmsHandler::build_discord_payload(msg);

    EXPECT_NE(payload.find("\"title\":\"SMS Received\""), std::string::npos);
    EXPECT_NE(payload.find("+1234567890"), std::string::npos);
    EXPECT_NE(payload.find("+9876543210"), std::string::npos);
    EXPECT_NE(payload.find("\"name\":\"To\""), std::string::npos);
    EXPECT_NE(payload.find("ec20-abc123"), std::string::npos);
    EXPECT_NE(payload.find("Hello world"), std::string::npos);
    EXPECT_NE(payload.find("\"color\":3447003"), std::string::npos);
}

TEST_F(DiscordPayloadTest, build_payload_without_receiver) {
    SmsMessage msg;
    msg.sender = "+1234567890";
    msg.body = "No receiver";
    msg.timestamp = "2026-05-03T14:30:00Z";
    msg.module_id = "ec20-abc123";

    std::string payload = SmsHandler::build_discord_payload(msg);

    EXPECT_EQ(payload.find("\"name\":\"To\""), std::string::npos);
    EXPECT_NE(payload.find("+1234567890"), std::string::npos);
}

TEST_F(DiscordPayloadTest, build_payload_escapes_special_chars) {
    SmsMessage msg;
    msg.sender = "+1234567890";
    msg.body = "Quote: \"hello\" and backslash: \\";
    msg.timestamp = "2026-05-03T14:30:00Z";
    msg.module_id = "ec20-test";

    std::string payload = SmsHandler::build_discord_payload(msg);

    EXPECT_NE(payload.find("\\\"hello\\\""), std::string::npos);
    EXPECT_NE(payload.find("\\\\"), std::string::npos);
}

TEST_F(DiscordPayloadTest, build_payload_escapes_newlines) {
    SmsMessage msg;
    msg.sender = "+1234567890";
    msg.body = "Line1\nLine2\rLine3";
    msg.timestamp = "2026-05-03T14:30:00Z";
    msg.module_id = "ec20-test";

    std::string payload = SmsHandler::build_discord_payload(msg);

    EXPECT_NE(payload.find("Line1\\nLine2\\rLine3"), std::string::npos);
}

TEST_F(DiscordPayloadTest, build_payload_empty_body) {
    SmsMessage msg;
    msg.sender = "+1234567890";
    msg.body = "";
    msg.timestamp = "2026-05-03T14:30:00Z";
    msg.module_id = "ec20-test";

    std::string payload = SmsHandler::build_discord_payload(msg);
    EXPECT_NE(payload.find("\"description\":\"\""), std::string::npos);
}

class SmsHandlerLifecycleTest : public ::testing::Test {};

TEST_F(SmsHandlerLifecycleTest, start_stop_with_disabled_config) {
    SmsConfig config;
    config.enabled = false;

    SmsHandler handler(config);
    EXPECT_TRUE(handler.start());
    handler.stop();
}

TEST_F(SmsHandlerLifecycleTest, start_stop_with_temp_db) {
    SmsConfig config;
    config.enabled = true;
    config.db_path = "/tmp/test_sms_handler_lifecycle.db";

    SmsHandler handler(config);
    EXPECT_TRUE(handler.start());
    handler.stop();

    std::remove(config.db_path.c_str());
}

TEST_F(SmsHandlerLifecycleTest, start_with_no_webhook_url) {
    SmsConfig config;
    config.enabled = true;
    config.discord_webhook_url = "";
    config.db_path = "/tmp/test_sms_no_webhook.db";

    SmsHandler handler(config);
    EXPECT_TRUE(handler.start());
    handler.stop();

    std::remove(config.db_path.c_str());
}
