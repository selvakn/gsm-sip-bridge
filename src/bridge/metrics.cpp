#include "bridge/metrics.h"
#include "logger.h"

#include <prometheus/counter.h>
#include <prometheus/exposer.h>
#include <prometheus/gauge.h>
#include <prometheus/histogram.h>
#include <prometheus/registry.h>

#include <memory>
#include <mutex>

namespace metrics {

static std::unique_ptr<prometheus::Exposer> g_exposer;
static std::shared_ptr<prometheus::Registry> g_registry;

static prometheus::Family<prometheus::Counter>* g_sip_registrations = nullptr;
static prometheus::Family<prometheus::Counter>* g_gsm_calls = nullptr;
static prometheus::Family<prometheus::Counter>* g_sip_calls = nullptr;
static prometheus::Family<prometheus::Counter>* g_module_init = nullptr;
static prometheus::Family<prometheus::Counter>* g_module_retries = nullptr;
static prometheus::Family<prometheus::Counter>* g_audio_errors = nullptr;

static prometheus::Family<prometheus::Gauge>* g_sip_registered = nullptr;
static prometheus::Family<prometheus::Gauge>* g_modules_active = nullptr;
static prometheus::Family<prometheus::Gauge>* g_modules_failed = nullptr;
static prometheus::Family<prometheus::Gauge>* g_active_calls = nullptr;
static prometheus::Family<prometheus::Gauge>* g_uptime = nullptr;

static prometheus::Family<prometheus::Counter>* g_sms_received = nullptr;
static prometheus::Family<prometheus::Counter>* g_sms_forwarded = nullptr;
static prometheus::Family<prometheus::Counter>* g_sms_db_writes = nullptr;

static prometheus::Family<prometheus::Histogram>* g_call_duration = nullptr;

void init(uint16_t port) {
    if (g_exposer) return;

    std::string bind_addr = "0.0.0.0:" + std::to_string(port);
    g_exposer = std::make_unique<prometheus::Exposer>(bind_addr);
    g_registry = std::make_shared<prometheus::Registry>();

    g_sip_registrations = &prometheus::BuildCounter()
        .Name("gsm_bridge_sip_registrations_total")
        .Help("SIP registration attempts")
        .Register(*g_registry);

    g_gsm_calls = &prometheus::BuildCounter()
        .Name("gsm_bridge_calls_total")
        .Help("Total GSM calls by status")
        .Register(*g_registry);

    g_sip_calls = &prometheus::BuildCounter()
        .Name("gsm_bridge_sip_calls_total")
        .Help("Total outbound SIP calls by status")
        .Register(*g_registry);

    g_module_init = &prometheus::BuildCounter()
        .Name("gsm_bridge_module_init_total")
        .Help("Module initialization attempts")
        .Register(*g_registry);

    g_module_retries = &prometheus::BuildCounter()
        .Name("gsm_bridge_module_retries_total")
        .Help("Module retry attempts")
        .Register(*g_registry);

    g_audio_errors = &prometheus::BuildCounter()
        .Name("gsm_bridge_audio_errors_total")
        .Help("Audio subsystem errors")
        .Register(*g_registry);

    g_sip_registered = &prometheus::BuildGauge()
        .Name("gsm_bridge_sip_registered")
        .Help("SIP registration state (1=registered, 0=unregistered)")
        .Register(*g_registry);

    g_modules_active = &prometheus::BuildGauge()
        .Name("gsm_bridge_modules_active")
        .Help("Number of active modules")
        .Register(*g_registry);

    g_modules_failed = &prometheus::BuildGauge()
        .Name("gsm_bridge_modules_failed")
        .Help("Number of failed modules pending retry")
        .Register(*g_registry);

    g_active_calls = &prometheus::BuildGauge()
        .Name("gsm_bridge_active_calls")
        .Help("Currently active bridged calls per module")
        .Register(*g_registry);

    g_uptime = &prometheus::BuildGauge()
        .Name("gsm_bridge_uptime_seconds")
        .Help("Process uptime in seconds")
        .Register(*g_registry);

    g_sms_received = &prometheus::BuildCounter()
        .Name("gsm_bridge_sms_received_total")
        .Help("Total SMS messages received")
        .Register(*g_registry);

    g_sms_forwarded = &prometheus::BuildCounter()
        .Name("gsm_bridge_sms_forwarded_total")
        .Help("SMS Discord forwarding outcomes")
        .Register(*g_registry);

    g_sms_db_writes = &prometheus::BuildCounter()
        .Name("gsm_bridge_sms_db_writes_total")
        .Help("SMS database write outcomes")
        .Register(*g_registry);

    g_call_duration = &prometheus::BuildHistogram()
        .Name("gsm_bridge_call_duration_seconds")
        .Help("Duration of completed bridged calls")
        .Register(*g_registry);

    g_exposer->RegisterCollectable(g_registry);
    LOG_INFO("metrics endpoint started on %s", bind_addr.c_str());
}

void shutdown() {
    g_exposer.reset();
    g_registry.reset();

    g_sip_registrations = nullptr;
    g_gsm_calls = nullptr;
    g_sip_calls = nullptr;
    g_module_init = nullptr;
    g_module_retries = nullptr;
    g_audio_errors = nullptr;
    g_sip_registered = nullptr;
    g_modules_active = nullptr;
    g_modules_failed = nullptr;
    g_active_calls = nullptr;
    g_uptime = nullptr;
    g_sms_received = nullptr;
    g_sms_forwarded = nullptr;
    g_sms_db_writes = nullptr;
    g_call_duration = nullptr;
}

void sip_registration(bool success) {
    if (!g_sip_registrations) return;
    g_sip_registrations->Add({{"status", success ? "success" : "failure"}}).Increment();
}

void sip_registered(bool is_registered) {
    if (!g_sip_registered) return;
    g_sip_registered->Add({}).Set(is_registered ? 1.0 : 0.0);
}

void gsm_call_incoming(const std::string& module_id, const std::string& caller_id) {
    if (!g_gsm_calls) return;
    g_gsm_calls->Add({{"module_id", module_id}, {"status", "incoming"},
                       {"caller_id", caller_id}}).Increment();
}

void gsm_call_answered(const std::string& module_id) {
    if (!g_gsm_calls) return;
    g_gsm_calls->Add({{"module_id", module_id}, {"status", "answered"},
                       {"caller_id", ""}}).Increment();
}

void gsm_call_missed(const std::string& module_id) {
    if (!g_gsm_calls) return;
    g_gsm_calls->Add({{"module_id", module_id}, {"status", "missed"},
                       {"caller_id", ""}}).Increment();
}

void sip_call_initiated(const std::string& module_id) {
    if (!g_sip_calls) return;
    g_sip_calls->Add({{"module_id", module_id}, {"status", "initiated"}}).Increment();
}

void sip_call_connected(const std::string& module_id) {
    if (!g_sip_calls) return;
    g_sip_calls->Add({{"module_id", module_id}, {"status", "connected"}}).Increment();
}

void sip_call_failed(const std::string& module_id, const std::string& reason) {
    if (!g_sip_calls) return;
    g_sip_calls->Add({{"module_id", module_id}, {"status", reason}}).Increment();
}

void call_ended(const std::string& module_id, double duration_seconds) {
    if (!g_call_duration) return;
    g_call_duration->Add({{"module_id", module_id}},
                         prometheus::Histogram::BucketBoundaries{
                             1, 5, 15, 30, 60, 120, 300, 600, 1800})
        .Observe(duration_seconds);
}

void module_init_success(const std::string& module_id) {
    if (!g_module_init) return;
    g_module_init->Add({{"module_id", module_id}, {"status", "success"}}).Increment();
}

void module_init_failure(const std::string& module_id) {
    if (!g_module_init) return;
    g_module_init->Add({{"module_id", module_id}, {"status", "failure"}}).Increment();
}

void module_retry(const std::string& module_id) {
    if (!g_module_retries) return;
    g_module_retries->Add({{"module_id", module_id}}).Increment();
}

void modules_active(int count) {
    if (!g_modules_active) return;
    g_modules_active->Add({}).Set(static_cast<double>(count));
}

void modules_failed(int count) {
    if (!g_modules_failed) return;
    g_modules_failed->Add({}).Set(static_cast<double>(count));
}

void active_calls_inc(const std::string& module_id) {
    if (!g_active_calls) return;
    g_active_calls->Add({{"module_id", module_id}}).Increment();
}

void active_calls_dec(const std::string& module_id) {
    if (!g_active_calls) return;
    g_active_calls->Add({{"module_id", module_id}}).Decrement();
}

void audio_error(const std::string& module_id, const std::string& type) {
    if (!g_audio_errors) return;
    g_audio_errors->Add({{"module_id", module_id}, {"type", type}}).Increment();
}

void uptime_update(double seconds) {
    if (!g_uptime) return;
    g_uptime->Add({}).Set(seconds);
}

void sms_received(const std::string& module_id) {
    if (!g_sms_received) return;
    g_sms_received->Add({{"module_id", module_id}}).Increment();
}

void sms_forwarded(const std::string& module_id, const std::string& status) {
    if (!g_sms_forwarded) return;
    g_sms_forwarded->Add({{"module_id", module_id}, {"status", status}}).Increment();
}

void sms_db_write(bool success) {
    if (!g_sms_db_writes) return;
    g_sms_db_writes->Add({{"status", success ? "success" : "failure"}}).Increment();
}

}  // namespace metrics
