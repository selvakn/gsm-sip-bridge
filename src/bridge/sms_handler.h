#pragma once

#include "bridge/bridge_config.h"
#include "bridge/sms_store.h"
#include "at_commander.h"

#include <atomic>
#include <condition_variable>
#include <mutex>
#include <queue>
#include <string>
#include <thread>

struct SmsMessage {
    std::string sender;
    std::string body;
    std::string timestamp;
    std::string module_id;
    std::string receiver;
};

class SmsHandler {
public:
    explicit SmsHandler(const SmsConfig& config);
    ~SmsHandler();

    SmsHandler(const SmsHandler&) = delete;
    SmsHandler& operator=(const SmsHandler&) = delete;

    bool start();
    void stop();

    void enable_sms_mode(AtCommander& at);
    bool handle_cmti(AtCommander& at, const std::string& urc,
                     const std::string& module_id, const std::string& receiver = "");

    static std::string build_discord_payload(const SmsMessage& msg);
    static bool parse_cmti(const std::string& urc, int& index);
    static bool parse_cmgr(const std::string& header, const std::string& body, SmsMessage& msg);

private:
    void forward_worker();
    bool post_to_discord(const SmsMessage& msg, int64_t record_id);

    SmsConfig config_;
    SmsStore store_;

    std::thread worker_;
    std::atomic<bool> running_{false};

    std::mutex queue_mutex_;
    std::condition_variable queue_cv_;

    struct ForwardTask {
        SmsMessage msg;
        int64_t record_id;
    };
    std::queue<ForwardTask> queue_;
};
