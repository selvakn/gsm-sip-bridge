#pragma once

#include "device_discovery.h"
#include "serial_port.h"
#include "at_commander.h"
#include "bridge/bridge_config.h"

#include <atomic>
#include <memory>
#include <string>
#include <thread>

class BridgeAccount;
class SmsHandler;
struct SipConfig;

enum class CardState {
    DISCOVERED,
    INITIALIZING,
    ACTIVE,
    FAILED,
    STOPPING,
    STOPPED
};

const char* card_state_str(CardState state);

std::string derive_card_id(const std::string& serial_number, const std::string& usb_path);

class CardInstance {
public:
    explicit CardInstance(DeviceInfo device);
    ~CardInstance();

    CardInstance(const CardInstance&) = delete;
    CardInstance& operator=(const CardInstance&) = delete;
    CardInstance(CardInstance&&) = delete;
    CardInstance& operator=(CardInstance&&) = delete;

    bool initialize(bool verbose);

    void start(BridgeAccount& account,
               const BridgeConfig& bridge_config,
               const SipConfig& sip_config,
               std::atomic<bool>& running,
               SmsHandler* sms_handler = nullptr);

    void stop();

    const std::string& card_id() const { return card_id_; }
    const std::string& own_number() const { return own_number_; }
    const DeviceInfo& device() const { return device_; }
    CardState state() const { return state_.load(std::memory_order_acquire); }
    const std::string& fail_reason() const { return fail_reason_; }

private:
    void run_loop(BridgeAccount& account,
                  const BridgeConfig& bridge_config,
                  const SipConfig& sip_config,
                  std::atomic<bool>& running,
                  SmsHandler* sms_handler);

    void handle_bridged_call(AtCommander& at,
                             BridgeAccount& account,
                             const std::string& sip_dest_uri,
                             uint16_t dial_timeout_sec,
                             const std::string& gsm_caller_id,
                             std::atomic<bool>& running);

    DeviceInfo device_;
    std::string card_id_;
    std::string own_number_;
    std::string fail_reason_;
    std::atomic<CardState> state_{CardState::DISCOVERED};
    SerialPort serial_;
    std::unique_ptr<AtCommander> at_;
    std::thread thread_;
    bool verbose_ = false;
};
