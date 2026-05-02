#include "bridge/bridge_account.h"
#include "bridge/bridge_call.h"
#include "logger.h"

BridgeAccount::BridgeAccount() = default;
BridgeAccount::~BridgeAccount() = default;

void BridgeAccount::onRegState(pj::OnRegStateParam& prm) {
    pj::AccountInfo ai = getInfo();

    if (ai.regIsActive) {
        LOG_INFO("SIP registration successful (code=%d)", prm.code);
        registered_.store(true, std::memory_order_release);
    } else {
        LOG_WARN("SIP registration lost (code=%d, reason=%s)",
                 prm.code, prm.reason.c_str());
        registered_.store(false, std::memory_order_release);
    }
}

void BridgeAccount::onIncomingCall(pj::OnIncomingCallParam& iprm) {
    pj::Call call(*this, iprm.callId);
    pj::CallInfo ci = call.getInfo();
    LOG_INFO("rejecting inbound SIP call from %s (bridge mode)", ci.remoteUri.c_str());

    pj::CallOpParam op;
    op.statusCode = PJSIP_SC_BUSY_HERE;
    call.hangup(op);
}

BridgeCall* BridgeAccount::make_outbound_call(const std::string& dest_uri,
                                              const std::string& gsm_caller_id) {
    auto call = std::make_unique<BridgeCall>(*this);

    try {
        pj::CallOpParam op(true);

        if (!gsm_caller_id.empty()) {
            pj::SipHeader pai_header;
            pai_header.hName = "P-Asserted-Identity";
            pai_header.hValue = "\"" + gsm_caller_id + "\" <tel:" + gsm_caller_id + ">";

            pj::SipHeader gsm_header;
            gsm_header.hName = "X-GSM-Caller-ID";
            gsm_header.hValue = gsm_caller_id;

            op.txOption.headers.push_back(pai_header);
            op.txOption.headers.push_back(gsm_header);

            LOG_INFO("forwarding GSM caller ID: %s", gsm_caller_id.c_str());
        }

        call->makeCall(dest_uri, op);
        LOG_INFO("outbound SIP call to %s", dest_uri.c_str());
    } catch (pj::Error& err) {
        LOG_ERROR("SIP call failed: %s", err.info().c_str());
        return nullptr;
    }

    active_call_ = std::move(call);
    return active_call_.get();
}

void BridgeAccount::hangup_call() {
    if (!active_call_) return;

    try {
        if (active_call_->isActive()) {
            pj::CallOpParam op;
            op.statusCode = PJSIP_SC_OK;
            active_call_->hangup(op);
        }
    } catch (pj::Error& err) {
        LOG_WARN("SIP hangup error: %s", err.info().c_str());
    }
}

void BridgeAccount::clear_call() {
    active_call_.reset();
}
