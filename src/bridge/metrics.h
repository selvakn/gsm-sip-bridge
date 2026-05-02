#pragma once

#include <cstdint>
#include <string>

namespace metrics {

static constexpr uint16_t DEFAULT_METRICS_PORT = 9091;

void init(uint16_t port = DEFAULT_METRICS_PORT);
void shutdown();

void sip_registration(bool success);
void sip_registered(bool is_registered);

void gsm_call_incoming(const std::string& module_id, const std::string& caller_id);
void gsm_call_answered(const std::string& module_id);
void gsm_call_missed(const std::string& module_id);

void sip_call_initiated(const std::string& module_id);
void sip_call_connected(const std::string& module_id);
void sip_call_failed(const std::string& module_id, const std::string& reason);

void call_ended(const std::string& module_id, double duration_seconds);

void module_init_success(const std::string& module_id);
void module_init_failure(const std::string& module_id);
void module_retry(const std::string& module_id);

void modules_active(int count);
void modules_failed(int count);
void active_calls_inc(const std::string& module_id);
void active_calls_dec(const std::string& module_id);

void audio_error(const std::string& module_id, const std::string& type);

void uptime_update(double seconds);

}  // namespace metrics
