#include "bridge/bridge_account.h"
#include "bridge/bridge_call.h"
#include "bridge/metrics.h"
#include "logger.h"

BridgeAccount::BridgeAccount() = default;
BridgeAccount::~BridgeAccount() = default;

void BridgeAccount::onRegState(pj::OnRegStateParam& prm) {
    pj::AccountInfo ai = getInfo();

    if (ai.regIsActive) {
        LOG_INFO("SIP registration successful (code=%d)", prm.code);
        registered_.store(true, std::memory_order_release);
        metrics::sip_registration(true);
        metrics::sip_registered(true);
    } else {
        LOG_WARN("SIP registration lost (code=%d, reason=%s)",
                 prm.code, prm.reason.c_str());
        registered_.store(false, std::memory_order_release);
        metrics::sip_registration(false);
        metrics::sip_registered(false);
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

    BridgeCall* raw = call.get();
    int id = raw->getId();

    std::lock_guard<std::mutex> lock(calls_mutex_);
    active_calls_[id] = std::move(call);
    return raw;
}

void BridgeAccount::hangup_call(int call_id) {
    std::lock_guard<std::mutex> lock(calls_mutex_);
    auto it = active_calls_.find(call_id);
    if (it == active_calls_.end()) return;

    try {
        if (it->second && it->second->isActive()) {
            pj::CallOpParam op;
            op.statusCode = PJSIP_SC_OK;
            it->second->hangup(op);
        }
    } catch (pj::Error& err) {
        LOG_WARN("SIP hangup error (call %d): %s", call_id, err.info().c_str());
    }
}

void BridgeAccount::hangup_all_calls() {
    std::lock_guard<std::mutex> lock(calls_mutex_);
    for (auto& [id, call] : active_calls_) {
        try {
            if (call && call->isActive()) {
                pj::CallOpParam op;
                op.statusCode = PJSIP_SC_OK;
                call->hangup(op);
            }
        } catch (pj::Error& err) {
            LOG_WARN("SIP hangup error (call %d): %s", id, err.info().c_str());
        }
    }
}

void BridgeAccount::remove_call(int call_id) {
    std::lock_guard<std::mutex> lock(calls_mutex_);
    active_calls_.erase(call_id);
}

void BridgeAccount::shutdown() {
    hangup_all_calls();
    {
        std::lock_guard<std::mutex> lock(calls_mutex_);
        active_calls_.clear();
    }
    try {
        pj::AccountConfig dummy;
        this->pj::Account::shutdown();
    } catch (...) {}
}
