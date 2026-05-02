#pragma once

#include <pjsua2.hpp>
#include <atomic>
#include <memory>

class EchoCall;

class EchoAccount : public pj::Account {
public:
    EchoAccount();
    ~EchoAccount() override;

    void onRegState(pj::OnRegStateParam& prm) override;
    void onIncomingCall(pj::OnIncomingCallParam& iprm) override;

    bool is_in_call() const;

private:
    std::unique_ptr<EchoCall> active_call_;
    std::atomic<bool> in_call_{false};

    void clear_call();

    friend class EchoCall;
};
