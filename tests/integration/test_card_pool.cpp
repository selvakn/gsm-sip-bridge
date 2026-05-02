#include "bridge/card_instance.h"
#include "bridge/card_pool.h"
#include "device_discovery.h"

#include <gtest/gtest.h>

TEST(CardId, derive_from_serial_number_uses_last_six_chars) {
    // Arrange
    std::string serial = "ABCDEF123456";

    // Act
    std::string id = derive_card_id(serial, "1-2.3");

    // Assert
    EXPECT_EQ(id, "ec20-123456");
}

TEST(CardId, derive_from_short_serial_number_uses_full_string) {
    // Arrange
    std::string serial = "ABC";

    // Act
    std::string id = derive_card_id(serial, "1-2.3");

    // Assert
    EXPECT_EQ(id, "ec20-ABC");
}

TEST(CardId, derive_falls_back_to_usb_path_when_no_serial) {
    // Arrange
    std::string serial = "";
    std::string usb_path = "1-2.3";

    // Act
    std::string id = derive_card_id(serial, usb_path);

    // Assert
    EXPECT_EQ(id, "ec20-1-2.3");
}

TEST(CardId, derive_with_exact_six_char_serial) {
    // Arrange
    std::string serial = "ABCDEF";

    // Act
    std::string id = derive_card_id(serial, "1-2.3");

    // Assert
    EXPECT_EQ(id, "ec20-ABCDEF");
}

TEST(CardState, state_str_returns_valid_strings) {
    // Assert
    EXPECT_STREQ(card_state_str(CardState::DISCOVERED), "DISCOVERED");
    EXPECT_STREQ(card_state_str(CardState::INITIALIZING), "INITIALIZING");
    EXPECT_STREQ(card_state_str(CardState::ACTIVE), "ACTIVE");
    EXPECT_STREQ(card_state_str(CardState::FAILED), "FAILED");
    EXPECT_STREQ(card_state_str(CardState::STOPPING), "STOPPING");
    EXPECT_STREQ(card_state_str(CardState::STOPPED), "STOPPED");
}

TEST(CardInstance, construct_with_device_info_assigns_card_id) {
    // Arrange
    DeviceInfo dev{"/dev/ttyUSB2", "hw:1,0", "SN123456789", "1-2.3"};

    // Act
    CardInstance card(std::move(dev));

    // Assert
    EXPECT_EQ(card.card_id(), "ec20-456789");
    EXPECT_EQ(card.state(), CardState::DISCOVERED);
    EXPECT_EQ(card.device().serial_port, "/dev/ttyUSB2");
    EXPECT_EQ(card.device().alsa_device, "hw:1,0");
}

TEST(CardInstance, initialize_fails_for_nonexistent_serial_port) {
    // Arrange
    DeviceInfo dev{"/dev/ttyNONEXISTENT_99", "hw:99,0", "TESTSERIAL", "99-99"};
    CardInstance card(std::move(dev));

    // Act
    bool result = card.initialize(false);

    // Assert
    EXPECT_FALSE(result);
    EXPECT_EQ(card.state(), CardState::FAILED);
    EXPECT_FALSE(card.fail_reason().empty());
}

TEST(CardPool, discover_returns_error_when_no_devices) {
    // Arrange — may or may not have EC20 connected
    // This test verifies the error path logic, not hardware detection
    CardPool pool;

    // Act
    auto result = pool.discover_and_initialize(false);

    // Assert — if no EC20 is connected, expect specific error
    if (!result.ok) {
        EXPECT_FALSE(result.error.empty());
    } else {
        EXPECT_GT(pool.active_count(), static_cast<size_t>(0));
    }
}

TEST(CardPool, counts_are_consistent) {
    // Arrange
    CardPool pool;
    auto result = pool.discover_and_initialize(false);

    // Assert
    EXPECT_EQ(pool.total_count(), pool.active_count() + pool.failed_count());
}
