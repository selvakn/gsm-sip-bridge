#include "bridge/card_pool.h"
#include "bridge/bridge_account.h"
#include "bridge/metrics.h"
#include "sip/sip_config.h"
#include "device_discovery.h"
#include "logger.h"

#include <chrono>

CardPool::~CardPool() {
    stop_all();
}

CardPool::DiscoverResult CardPool::discover_and_initialize(bool verbose) {
    verbose_ = verbose;
    auto devices = discover_all_ec20();

    if (devices.empty()) {
        return {false, "no EC20 modules found (USB " +
                std::string("2c7c:0125") + ")"};
    }

    LOG_INFO("detected %zu EC20 module(s)", devices.size());

    for (auto& dev : devices) {
        auto card = std::make_unique<CardInstance>(std::move(dev));
        std::string cid = card->card_id();
        if (card->initialize(verbose)) {
            metrics::module_init_success(cid);
            std::lock_guard<std::mutex> lock(mutex_);
            active_cards_.push_back(std::move(card));
        } else {
            metrics::module_init_failure(cid);
            std::lock_guard<std::mutex> lock(mutex_);
            failed_cards_.push_back(std::move(card));
        }
    }

    metrics::modules_active(static_cast<int>(active_cards_.size()));
    metrics::modules_failed(static_cast<int>(failed_cards_.size()));

    if (active_cards_.empty()) {
        return {false, "all " + std::to_string(failed_cards_.size()) +
                " EC20 module(s) failed initialization"};
    }

    return {true, ""};
}

void CardPool::print_summary() const {
    std::lock_guard<std::mutex> lock(mutex_);
    LOG_INFO("=== Module Summary ===");
    for (const auto& card : active_cards_) {
        LOG_INFO("  [%s] serial=%s audio=%s — ACTIVE",
                 card->card_id().c_str(),
                 card->device().serial_port.c_str(),
                 card->device().alsa_device.c_str());
    }
    for (const auto& card : failed_cards_) {
        LOG_INFO("  [%s] serial=%s audio=%s — FAILED (%s)",
                 card->card_id().c_str(),
                 card->device().serial_port.c_str(),
                 card->device().alsa_device.c_str(),
                 card->fail_reason().c_str());
    }
    LOG_INFO("ready, %zu module(s) active, %zu failed",
             active_cards_.size(), failed_cards_.size());
}

void CardPool::start_all(BridgeAccount& account,
                         const BridgeConfig& bridge_config,
                         const SipConfig& sip_config,
                         std::atomic<bool>& running) {
    std::lock_guard<std::mutex> lock(mutex_);
    for (auto& card : active_cards_) {
        card->start(account, bridge_config, sip_config, running);
    }
}

void CardPool::start_retry_thread(BridgeAccount& account,
                                  const BridgeConfig& bridge_config,
                                  const SipConfig& sip_config,
                                  std::atomic<bool>& running) {
    std::lock_guard<std::mutex> lock(mutex_);
    if (failed_cards_.empty()) return;

    retry_thread_ = std::thread(&CardPool::retry_loop, this,
                                std::ref(account), std::cref(bridge_config),
                                std::cref(sip_config), std::ref(running));
}

void CardPool::stop_all() {
    if (retry_thread_.joinable()) {
        retry_thread_.join();
    }

    std::lock_guard<std::mutex> lock(mutex_);
    for (auto& card : active_cards_) {
        card->stop();
    }
    active_cards_.clear();
    failed_cards_.clear();
}

size_t CardPool::active_count() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return active_cards_.size();
}

size_t CardPool::failed_count() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return failed_cards_.size();
}

size_t CardPool::total_count() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return active_cards_.size() + failed_cards_.size();
}

void CardPool::retry_loop(BridgeAccount& account,
                          const BridgeConfig& bridge_config,
                          const SipConfig& sip_config,
                          std::atomic<bool>& running) {
    LOG_INFO("retry thread started (%us interval)", DEFAULT_RETRY_INTERVAL_SEC);

    while (running.load(std::memory_order_relaxed)) {
        for (unsigned int i = 0; i < DEFAULT_RETRY_INTERVAL_SEC; ++i) {
            if (!running.load(std::memory_order_relaxed)) return;
            std::this_thread::sleep_for(std::chrono::seconds(1));
        }

        std::lock_guard<std::mutex> lock(mutex_);
        if (failed_cards_.empty()) {
            LOG_INFO("retry thread: no more failed modules, exiting");
            return;
        }

        auto it = failed_cards_.begin();
        while (it != failed_cards_.end()) {
            std::string cid = (*it)->card_id();
            metrics::module_retry(cid);
            LOG_INFO("[%s] retrying initialization...", cid.c_str());
            if ((*it)->initialize(verbose_)) {
                LOG_INFO("[%s] retry succeeded, adding to active pool", cid.c_str());
                metrics::module_init_success(cid);
                (*it)->start(account, bridge_config, sip_config, running);
                active_cards_.push_back(std::move(*it));
                it = failed_cards_.erase(it);
            } else {
                metrics::module_init_failure(cid);
                LOG_WARN("[%s] retry failed: %s", cid.c_str(),
                         (*it)->fail_reason().c_str());
                ++it;
            }
        }
        metrics::modules_active(static_cast<int>(active_cards_.size()));
        metrics::modules_failed(static_cast<int>(failed_cards_.size()));
    }
}
