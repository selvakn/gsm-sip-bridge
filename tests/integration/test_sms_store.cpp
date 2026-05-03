#include <gtest/gtest.h>
#include "bridge/sms_store.h"

#include <cstdio>
#include <string>

static const std::string TEST_DB_PATH = "/tmp/test_sms_store.db";

class SmsStoreTest : public ::testing::Test {
protected:
    void SetUp() override {
        std::remove(TEST_DB_PATH.c_str());
        ASSERT_TRUE(store_.open(TEST_DB_PATH));
    }

    void TearDown() override {
        store_.close();
        std::remove(TEST_DB_PATH.c_str());
    }

    SmsStore store_;
};

TEST_F(SmsStoreTest, open_creates_database) {
    EXPECT_EQ(store_.count(), 0);
}

TEST_F(SmsStoreTest, insert_returns_positive_id) {
    SmsRecord rec;
    rec.sender = "+1234567890";
    rec.body = "Hello world";
    rec.received_at = "2026-05-03T14:30:00Z";
    rec.module_id = "ec20-abc123";

    int64_t id = store_.insert(rec);
    EXPECT_GT(id, 0);
    EXPECT_EQ(store_.count(), 1);
}

TEST_F(SmsStoreTest, insert_multiple_records) {
    for (int i = 0; i < 5; ++i) {
        SmsRecord rec;
        rec.sender = "+100000000" + std::to_string(i);
        rec.body = "msg " + std::to_string(i);
        rec.received_at = "2026-05-03T14:3" + std::to_string(i) + ":00Z";
        rec.module_id = "ec20-test";
        EXPECT_GT(store_.insert(rec), 0);
    }
    EXPECT_EQ(store_.count(), 5);
}

TEST_F(SmsStoreTest, fetch_pending_returns_inserted_records) {
    SmsRecord rec;
    rec.sender = "+1234567890";
    rec.body = "Test message";
    rec.received_at = "2026-05-03T14:30:00Z";
    rec.module_id = "ec20-abc123";
    rec.discord_status = "pending";

    store_.insert(rec);

    auto pending = store_.fetch_pending();
    ASSERT_EQ(pending.size(), 1u);
    EXPECT_EQ(pending[0].sender, "+1234567890");
    EXPECT_EQ(pending[0].body, "Test message");
    EXPECT_EQ(pending[0].module_id, "ec20-abc123");
    EXPECT_EQ(pending[0].discord_status, "pending");
}

TEST_F(SmsStoreTest, update_discord_status_to_sent) {
    SmsRecord rec;
    rec.sender = "+1234567890";
    rec.body = "Test";
    rec.received_at = "2026-05-03T14:30:00Z";
    rec.module_id = "ec20-abc123";

    int64_t id = store_.insert(rec);
    ASSERT_GT(id, 0);

    EXPECT_TRUE(store_.update_discord_status(id, "sent", "2026-05-03T14:30:01Z"));

    auto pending = store_.fetch_pending();
    EXPECT_EQ(pending.size(), 0u);
}

TEST_F(SmsStoreTest, update_discord_status_to_failed) {
    SmsRecord rec;
    rec.sender = "+1234567890";
    rec.body = "Test";
    rec.received_at = "2026-05-03T14:30:00Z";
    rec.module_id = "ec20-abc123";

    int64_t id = store_.insert(rec);
    EXPECT_TRUE(store_.update_discord_status(id, "failed"));

    auto pending = store_.fetch_pending();
    EXPECT_EQ(pending.size(), 0u);
}

TEST_F(SmsStoreTest, insert_with_empty_body) {
    SmsRecord rec;
    rec.sender = "+1234567890";
    rec.body = "";
    rec.received_at = "2026-05-03T14:30:00Z";
    rec.module_id = "ec20-abc123";

    int64_t id = store_.insert(rec);
    EXPECT_GT(id, 0);

    auto pending = store_.fetch_pending();
    ASSERT_EQ(pending.size(), 1u);
    EXPECT_EQ(pending[0].body, "");
}

TEST_F(SmsStoreTest, insert_with_unicode_body) {
    SmsRecord rec;
    rec.sender = "+1234567890";
    rec.body = u8"Привет мир 🌍";
    rec.received_at = "2026-05-03T14:30:00Z";
    rec.module_id = "ec20-abc123";

    int64_t id = store_.insert(rec);
    EXPECT_GT(id, 0);

    auto pending = store_.fetch_pending();
    ASSERT_EQ(pending.size(), 1u);
    EXPECT_EQ(pending[0].body, u8"Привет мир 🌍");
}

TEST_F(SmsStoreTest, fetch_pending_limit) {
    for (int i = 0; i < 10; ++i) {
        SmsRecord rec;
        rec.sender = "+100000000" + std::to_string(i);
        rec.body = "msg";
        rec.received_at = "2026-05-03T14:30:00Z";
        rec.module_id = "ec20-test";
        store_.insert(rec);
    }

    auto pending = store_.fetch_pending(3);
    EXPECT_EQ(pending.size(), 3u);
}

TEST_F(SmsStoreTest, insert_with_skipped_status) {
    SmsRecord rec;
    rec.sender = "+1234567890";
    rec.body = "No webhook";
    rec.received_at = "2026-05-03T14:30:00Z";
    rec.module_id = "ec20-abc123";
    rec.discord_status = "skipped";

    int64_t id = store_.insert(rec);
    EXPECT_GT(id, 0);

    auto pending = store_.fetch_pending();
    EXPECT_EQ(pending.size(), 0u);
}

TEST_F(SmsStoreTest, operations_on_closed_store) {
    SmsStore closed_store;

    SmsRecord rec;
    rec.sender = "+1234567890";
    rec.body = "test";
    rec.received_at = "2026-05-03T14:30:00Z";
    rec.module_id = "ec20-test";

    EXPECT_EQ(closed_store.insert(rec), -1);
    EXPECT_FALSE(closed_store.update_discord_status(1, "sent"));
    EXPECT_EQ(closed_store.fetch_pending().size(), 0u);
    EXPECT_EQ(closed_store.count(), 0);
}
