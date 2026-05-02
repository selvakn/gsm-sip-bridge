#include <gtest/gtest.h>
#include "bridge/bridge_account.h"
#include "bridge/bridge_call.h"
#include "bridge/alsa_media_port.h"
#include "ring_buffer.h"
#include <pjsua2.hpp>
#include <thread>

class BridgeCallTest : public ::testing::Test {
protected:
    static pj::Endpoint* ep_;
    static bool ep_initialized_;

    static void SetUpTestSuite() {
        if (ep_initialized_) return;
        ep_ = new pj::Endpoint();
        try {
            ep_->libCreate();
            pj::EpConfig cfg;
            cfg.logConfig.level = 0;
            cfg.logConfig.consoleLevel = 0;
            ep_->libInit(cfg);

            pj::TransportConfig tp;
            tp.port = 15060;
            ep_->transportCreate(PJSIP_TRANSPORT_UDP, tp);
            ep_->libStart();
            ep_->audDevManager().setNullDev();
            ep_initialized_ = true;
        } catch (pj::Error& err) {
            FAIL() << "PJSIP init failed: " << err.info();
        }
    }

    static void TearDownTestSuite() {
        if (ep_) {
            try { ep_->libDestroy(); } catch (...) {}
            delete ep_;
            ep_ = nullptr;
            ep_initialized_ = false;
        }
    }
};

pj::Endpoint* BridgeCallTest::ep_ = nullptr;
bool BridgeCallTest::ep_initialized_ = false;

TEST_F(BridgeCallTest, endpoint_initializes_with_null_audio) {
    // Arrange / Act / Assert
    EXPECT_TRUE(ep_initialized_);
}

TEST_F(BridgeCallTest, account_creates_successfully) {
    // Arrange
    BridgeAccount account;
    pj::AccountConfig cfg;
    cfg.idUri = "sip:test@localhost";

    // Act / Assert
    EXPECT_NO_THROW(account.create(cfg));
    EXPECT_NO_THROW(account.shutdown());
}

TEST_F(BridgeCallTest, account_starts_unregistered) {
    // Arrange
    BridgeAccount account;
    pj::AccountConfig cfg;
    cfg.idUri = "sip:test@localhost";
    account.create(cfg);

    // Act / Assert
    EXPECT_FALSE(account.is_registered());
    account.shutdown();
}

TEST_F(BridgeCallTest, media_port_creates_successfully) {
    // Arrange
    RingBuffer<int16_t> cap_buf(1600);
    RingBuffer<int16_t> play_buf(1600);
    AlsaMediaPort port(cap_buf, play_buf);

    // Act / Assert
    EXPECT_NO_THROW(port.create());
}

TEST_F(BridgeCallTest, media_port_provides_silence_when_buffer_empty) {
    // Arrange
    RingBuffer<int16_t> cap_buf(1600);
    RingBuffer<int16_t> play_buf(1600);
    AlsaMediaPort port(cap_buf, play_buf);
    port.create();

    pj::MediaFrame frame;
    frame.buf.resize(320);
    frame.size = 320;

    // Act
    port.onFrameRequested(frame);

    // Assert
    auto* samples = reinterpret_cast<int16_t*>(frame.buf.data());
    for (size_t i = 0; i < 160; ++i) {
        EXPECT_EQ(samples[i], 0);
    }
    EXPECT_EQ(frame.type, PJMEDIA_FRAME_TYPE_AUDIO);
}

TEST_F(BridgeCallTest, make_outbound_call_with_caller_id_does_not_crash) {
    // Arrange
    BridgeAccount account;
    pj::AccountConfig cfg;
    cfg.idUri = "sip:test@localhost";
    account.create(cfg);

    // Act — call will fail (no remote server) but header setup must not crash
    BridgeCall* call = account.make_outbound_call(
        "sip:999@127.0.0.1:19999", "+919876543210");

    // Assert
    if (call) {
        account.hangup_call(call->getId());
        account.remove_call(call->getId());
    }
    account.shutdown();
}

TEST_F(BridgeCallTest, make_outbound_call_without_caller_id_does_not_crash) {
    // Arrange
    BridgeAccount account;
    pj::AccountConfig cfg;
    cfg.idUri = "sip:test@localhost";
    account.create(cfg);

    // Act
    BridgeCall* call = account.make_outbound_call("sip:999@127.0.0.1:19999");

    // Assert
    if (call) {
        account.hangup_call(call->getId());
        account.remove_call(call->getId());
    }
    account.shutdown();
}

TEST_F(BridgeCallTest, media_port_returns_data_from_capture_buffer) {
    // Arrange
    RingBuffer<int16_t> cap_buf(1600);
    RingBuffer<int16_t> play_buf(1600);
    AlsaMediaPort port(cap_buf, play_buf);
    port.create();

    int16_t test_data[160];
    for (int i = 0; i < 160; ++i) test_data[i] = static_cast<int16_t>(i * 100);
    cap_buf.try_write(test_data, 160);

    pj::MediaFrame frame;
    frame.buf.resize(320);
    frame.size = 320;

    // Act
    port.onFrameRequested(frame);

    // Assert
    auto* samples = reinterpret_cast<int16_t*>(frame.buf.data());
    for (int i = 0; i < 160; ++i) {
        EXPECT_EQ(samples[i], test_data[i]);
    }
}
