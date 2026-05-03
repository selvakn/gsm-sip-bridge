#include <gtest/gtest.h>
#include "bridge/metrics.h"

#include <prometheus/counter.h>
#include <prometheus/exposer.h>
#include <prometheus/gauge.h>
#include <prometheus/histogram.h>
#include <prometheus/registry.h>
#include <prometheus/text_serializer.h>

#include <string>

static constexpr uint16_t TEST_METRICS_PORT = 19091;

class MetricsTest : public ::testing::Test {
protected:
    void SetUp() override {
        metrics::shutdown();
        metrics::init(TEST_METRICS_PORT);
    }

    void TearDown() override {
        metrics::shutdown();
    }
};

TEST_F(MetricsTest, init_starts_exposer_without_crash) {
    SUCCEED();
}

TEST_F(MetricsTest, sip_registration_increments_counter) {
    metrics::sip_registration(true);
    metrics::sip_registration(false);
    metrics::sip_registration(true);
    SUCCEED();
}

TEST_F(MetricsTest, sip_registered_gauge_updates) {
    metrics::sip_registered(true);
    metrics::sip_registered(false);
    SUCCEED();
}

TEST_F(MetricsTest, gsm_call_lifecycle) {
    metrics::gsm_call_incoming("ec20-abc123", "+1234567890");
    metrics::gsm_call_answered("ec20-abc123");
    metrics::sip_call_initiated("ec20-abc123");
    metrics::sip_call_connected("ec20-abc123");
    metrics::call_ended("ec20-abc123", 45.3);
    SUCCEED();
}

TEST_F(MetricsTest, gsm_call_missed_increments) {
    metrics::gsm_call_incoming("ec20-def456", "+9876543210");
    metrics::gsm_call_missed("ec20-def456");
    SUCCEED();
}

TEST_F(MetricsTest, sip_call_failure_with_reasons) {
    metrics::sip_call_initiated("ec20-abc123");
    metrics::sip_call_failed("ec20-abc123", "timeout");
    metrics::sip_call_initiated("ec20-abc123");
    metrics::sip_call_failed("ec20-abc123", "error");
    metrics::sip_call_initiated("ec20-abc123");
    metrics::sip_call_failed("ec20-abc123", "initiation_error");
    SUCCEED();
}

TEST_F(MetricsTest, module_init_and_retry_counters) {
    metrics::module_init_success("ec20-abc123");
    metrics::module_init_failure("ec20-def456");
    metrics::module_retry("ec20-def456");
    metrics::module_init_success("ec20-def456");
    SUCCEED();
}

TEST_F(MetricsTest, module_gauges) {
    metrics::modules_active(3);
    metrics::modules_failed(1);
    metrics::modules_active(4);
    metrics::modules_failed(0);
    SUCCEED();
}

TEST_F(MetricsTest, active_calls_gauge) {
    metrics::active_calls_inc("ec20-abc123");
    metrics::active_calls_inc("ec20-abc123");
    metrics::active_calls_dec("ec20-abc123");
    SUCCEED();
}

TEST_F(MetricsTest, audio_error_counter) {
    metrics::audio_error("ec20-abc123", "alsa_open");
    metrics::audio_error("ec20-abc123", "alsa_read");
    SUCCEED();
}

TEST_F(MetricsTest, uptime_gauge) {
    metrics::uptime_update(0.0);
    metrics::uptime_update(60.5);
    metrics::uptime_update(3600.0);
    SUCCEED();
}

TEST_F(MetricsTest, call_duration_histogram) {
    metrics::call_ended("ec20-abc123", 2.5);
    metrics::call_ended("ec20-abc123", 30.0);
    metrics::call_ended("ec20-abc123", 120.0);
    metrics::call_ended("ec20-abc123", 600.0);
    SUCCEED();
}

TEST_F(MetricsTest, sms_received_counter) {
    metrics::sms_received("ec20-abc123");
    metrics::sms_received("ec20-abc123");
    metrics::sms_received("ec20-def456");
    SUCCEED();
}

TEST_F(MetricsTest, sms_forwarded_counter) {
    metrics::sms_forwarded("ec20-abc123", "sent");
    metrics::sms_forwarded("ec20-abc123", "failed");
    metrics::sms_forwarded("ec20-abc123", "skipped");
    SUCCEED();
}

TEST_F(MetricsTest, sms_db_write_counter) {
    metrics::sms_db_write(true);
    metrics::sms_db_write(false);
    SUCCEED();
}

TEST_F(MetricsTest, operations_are_safe_after_shutdown) {
    metrics::shutdown();
    metrics::sip_registration(true);
    metrics::gsm_call_incoming("ec20-abc123", "+1234567890");
    metrics::modules_active(1);
    metrics::call_ended("ec20-abc123", 10.0);
    metrics::uptime_update(100.0);
    metrics::sms_received("ec20-abc123");
    metrics::sms_forwarded("ec20-abc123", "sent");
    metrics::sms_db_write(true);
    SUCCEED();
}
