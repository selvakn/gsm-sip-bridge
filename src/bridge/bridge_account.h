#pragma once

#include <pjsua2.hpp>
#include <atomic>
#include <map>
#include <memory>
#include <mutex>

class BridgeCall;

class BridgeAccount : public pj::Account {
public:
    BridgeAccount();
    ~BridgeAccount() override;

    void onRegState(pj::OnRegStateParam& prm) override;
    void onIncomingCall(pj::OnIncomingCallParam& iprm) override;

    BridgeCall* make_outbound_call(const std::string& dest_uri,
                                   const std::string& gsm_caller_id = "");
    void hangup_call(int call_id);
    void hangup_all_calls();
    void remove_call(int call_id);

    bool is_registered() const {
        return registered_.load(std::memory_order_acquire);
    }

    void shutdown();

private:
    std::mutex calls_mutex_;
    std::map<int, std::unique_ptr<BridgeCall>> active_calls_;
    std::atomic<bool> registered_{false};
};
