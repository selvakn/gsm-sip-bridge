#pragma once

#include <pjsua2.hpp>
#include <chrono>

class EchoAccount;

class EchoCall : public pj::Call {
public:
    EchoCall(EchoAccount& owner, int call_id = PJSUA_INVALID_ID);

    void onCallState(pj::OnCallStateParam& prm) override;
    void onCallMediaState(pj::OnCallMediaStateParam& prm) override;

private:
    EchoAccount& owner_;
    std::chrono::steady_clock::time_point answer_time_;
};
