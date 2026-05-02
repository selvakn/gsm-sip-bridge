#pragma once

#include <pjsua2.hpp>
#include <atomic>
#include <memory>

class BridgeCall;

class BridgeAccount : public pj::Account {
public:
    BridgeAccount();
    ~BridgeAccount() override;

    void onRegState(pj::OnRegStateParam& prm) override;
    void onIncomingCall(pj::OnIncomingCallParam& iprm) override;

    BridgeCall* make_outbound_call(const std::string& dest_uri,
                                   const std::string& gsm_caller_id = "");
    void hangup_call();
    void clear_call();

    bool is_registered() const {
        return registered_.load(std::memory_order_acquire);
    }

    BridgeCall* active_call() const { return active_call_.get(); }

private:
    std::unique_ptr<BridgeCall> active_call_;
    std::atomic<bool> registered_{false};
};
