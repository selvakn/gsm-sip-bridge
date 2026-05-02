#include "sip/echo_account.h"
#include "sip/echo_call.h"
#include "logger.h"

EchoAccount::EchoAccount() = default;
EchoAccount::~EchoAccount() = default;

void EchoAccount::onRegState(pj::OnRegStateParam& prm) {
    pj::AccountInfo ai = getInfo();

    if (ai.regIsActive) {
        LOG_INFO("SIP registration successful (code=%d)", prm.code);
    } else {
        LOG_WARN("SIP registration lost (code=%d, reason=%s)",
                 prm.code, prm.reason.c_str());
    }
}

void EchoAccount::onIncomingCall(pj::OnIncomingCallParam& iprm) {
    auto* call = new EchoCall(*this, iprm.callId);
    pj::CallInfo ci = call->getInfo();
    LOG_INFO("incoming call from %s", ci.remoteUri.c_str());

    if (in_call_.load(std::memory_order_relaxed)) {
        LOG_INFO("busy, rejecting call");
        pj::CallOpParam op;
        op.statusCode = PJSIP_SC_BUSY_HERE;
        call->hangup(op);
        delete call;
        return;
    }

    in_call_.store(true, std::memory_order_relaxed);
    active_call_.reset(call);

    pj::CallOpParam op;
    op.statusCode = PJSIP_SC_OK;
    call->answer(op);
}

bool EchoAccount::is_in_call() const {
    return in_call_.load(std::memory_order_relaxed);
}

void EchoAccount::clear_call() {
    active_call_.reset();
    in_call_.store(false, std::memory_order_relaxed);
}
