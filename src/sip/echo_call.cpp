#include "sip/echo_call.h"
#include "sip/echo_account.h"
#include "logger.h"

EchoCall::EchoCall(EchoAccount& owner, int call_id)
    : pj::Call(owner, call_id), owner_(owner) {}

void EchoCall::onCallState(pj::OnCallStateParam& /*prm*/) {
    pj::CallInfo ci = getInfo();

    if (ci.state == PJSIP_INV_STATE_CONFIRMED) {
        answer_time_ = std::chrono::steady_clock::now();
        LOG_INFO("call answered, echo active");
    } else if (ci.state == PJSIP_INV_STATE_DISCONNECTED) {
        auto duration = std::chrono::duration_cast<std::chrono::seconds>(
            std::chrono::steady_clock::now() - answer_time_).count();
        LOG_INFO("call ended (duration: %lds)", duration);

        owner_.clear_call();
    }
}

void EchoCall::onCallMediaState(pj::OnCallMediaStateParam& /*prm*/) {
    pj::CallInfo ci = getInfo();

    for (unsigned i = 0; i < ci.media.size(); ++i) {
        if (ci.media[i].type != PJMEDIA_TYPE_AUDIO) continue;
        if (ci.media[i].status != PJSUA_CALL_MEDIA_ACTIVE) continue;

        pj::AudioMedia aud_med = getAudioMedia(i);
        aud_med.startTransmit(aud_med);
        LOG_INFO("audio echo loopback connected on media index %u", i);
        return;
    }
}
