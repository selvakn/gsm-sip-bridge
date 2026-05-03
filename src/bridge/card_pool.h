#pragma once

#include "bridge/card_instance.h"
#include "bridge/bridge_config.h"

#include <atomic>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

class BridgeAccount;
class SmsHandler;
struct SipConfig;

class CardPool {
public:
    static constexpr unsigned int DEFAULT_RETRY_INTERVAL_SEC = 30;

    CardPool() = default;
    ~CardPool();

    CardPool(const CardPool&) = delete;
    CardPool& operator=(const CardPool&) = delete;

    struct DiscoverResult {
        bool ok = false;
        std::string error;
    };

    DiscoverResult discover_and_initialize(bool verbose);

    void print_summary() const;

    void start_all(BridgeAccount& account,
                   const BridgeConfig& bridge_config,
                   const SipConfig& sip_config,
                   std::atomic<bool>& running,
                   SmsHandler* sms_handler = nullptr);

    void start_retry_thread(BridgeAccount& account,
                            const BridgeConfig& bridge_config,
                            const SipConfig& sip_config,
                            std::atomic<bool>& running,
                            SmsHandler* sms_handler = nullptr);

    void stop_all();

    size_t active_count() const;
    size_t failed_count() const;
    size_t total_count() const;

private:
    void retry_loop(BridgeAccount& account,
                    const BridgeConfig& bridge_config,
                    const SipConfig& sip_config,
                    std::atomic<bool>& running,
                    SmsHandler* sms_handler);

    mutable std::mutex mutex_;
    std::vector<std::unique_ptr<CardInstance>> active_cards_;
    std::vector<std::unique_ptr<CardInstance>> failed_cards_;
    std::thread retry_thread_;
    bool verbose_ = false;
};
