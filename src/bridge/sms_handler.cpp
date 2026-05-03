#include "bridge/sms_handler.h"
#include "bridge/metrics.h"
#include "logger.h"

#include <algorithm>
#include <chrono>
#include <ctime>
#include <sstream>

#define CPPHTTPLIB_OPENSSL_SUPPORT
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wdeprecated-declarations"
#include <httplib.h>
#pragma GCC diagnostic pop

static constexpr int CMGR_RESPONSE_TIMEOUT_MS = 5000;
static constexpr int DISCORD_TIMEOUT_SEC = 10;
static constexpr int DISCORD_EMBED_COLOR = 3447003;

static std::string utc_now_iso8601() {
    auto now = std::chrono::system_clock::now();
    std::time_t t = std::chrono::system_clock::to_time_t(now);
    struct tm tm_buf{};
    gmtime_r(&t, &tm_buf);
    char buf[32];
    std::strftime(buf, sizeof(buf), "%Y-%m-%dT%H:%M:%SZ", &tm_buf);
    return buf;
}

static std::string escape_json(const std::string& s) {
    std::string out;
    out.reserve(s.size() + 16);
    for (char c : s) {
        switch (c) {
            case '"':  out += "\\\""; break;
            case '\\': out += "\\\\"; break;
            case '\n': out += "\\n";  break;
            case '\r': out += "\\r";  break;
            case '\t': out += "\\t";  break;
            default:
                if (static_cast<unsigned char>(c) < 0x20) {
                    char hex[8];
                    std::snprintf(hex, sizeof(hex), "\\u%04x", static_cast<unsigned char>(c));
                    out += hex;
                } else {
                    out += c;
                }
        }
    }
    return out;
}

SmsHandler::SmsHandler(const SmsConfig& config)
    : config_(config) {}

SmsHandler::~SmsHandler() {
    stop();
}

bool SmsHandler::start() {
    if (!config_.enabled) {
        LOG_INFO("sms_handler: disabled by config");
        return true;
    }

    if (!store_.open(config_.db_path)) {
        LOG_ERROR("sms_handler: failed to open SMS database at %s", config_.db_path.c_str());
        return false;
    }

    running_.store(true, std::memory_order_release);
    worker_ = std::thread(&SmsHandler::forward_worker, this);

    LOG_INFO("sms_handler: started (webhook=%s, db=%s)",
             config_.discord_webhook_url.empty() ? "disabled" : "configured",
             config_.db_path.c_str());
    return true;
}

void SmsHandler::stop() {
    running_.store(false, std::memory_order_release);
    queue_cv_.notify_all();

    if (worker_.joinable()) {
        worker_.join();
    }

    store_.close();
}

void SmsHandler::enable_sms_mode(AtCommander& at) {
    at.send_and_expect_ok("AT+CMGF=1", 2000);
    at.send_and_expect_ok("AT+CNMI=2,1,0,0,0", 2000);
    at.send_and_expect_ok("AT+CPMS=\"ME\",\"ME\",\"ME\"", 2000);
}

bool SmsHandler::parse_cmti(const std::string& urc, int& index) {
    auto pos = urc.find("+CMTI:");
    if (pos == std::string::npos) return false;

    auto comma = urc.find(',', pos);
    if (comma == std::string::npos) return false;

    try {
        index = std::stoi(urc.substr(comma + 1));
        return true;
    } catch (...) {
        return false;
    }
}

bool SmsHandler::parse_cmgr(const std::string& header, const std::string& body, SmsMessage& msg) {
    auto pos = header.find("+CMGR:");
    if (pos == std::string::npos) return false;

    auto first_quote = header.find('"', pos);
    if (first_quote == std::string::npos) return false;
    auto second_quote = header.find('"', first_quote + 1);
    if (second_quote == std::string::npos) return false;

    auto third_quote = header.find('"', second_quote + 1);
    if (third_quote == std::string::npos) return false;
    auto fourth_quote = header.find('"', third_quote + 1);
    if (fourth_quote == std::string::npos) return false;

    msg.sender = header.substr(third_quote + 1, fourth_quote - third_quote - 1);

    auto ts_start = header.rfind('"');
    if (ts_start == std::string::npos) return false;
    auto ts_end_quote = header.rfind('"', ts_start - 1);
    if (ts_end_quote == std::string::npos || ts_end_quote == ts_start) return false;

    size_t fifth_quote = std::string::npos;
    size_t sixth_quote = std::string::npos;
    size_t count = 0;
    for (size_t i = pos; i < header.size(); ++i) {
        if (header[i] == '"') {
            count++;
            if (count == 7) fifth_quote = i;
            if (count == 8) { sixth_quote = i; break; }
        }
    }

    if (fifth_quote != std::string::npos && sixth_quote != std::string::npos) {
        msg.timestamp = header.substr(fifth_quote + 1, sixth_quote - fifth_quote - 1);
    }

    msg.body = body;
    while (!msg.body.empty() && (msg.body.back() == '\r' || msg.body.back() == '\n')) {
        msg.body.pop_back();
    }

    return !msg.sender.empty();
}

bool SmsHandler::handle_cmti(AtCommander& at, const std::string& urc,
                             const std::string& module_id, const std::string& receiver) {
    int index = -1;
    if (!parse_cmti(urc, index)) return false;

    LOG_INFO("[%s] SMS notification: index=%d", module_id.c_str(), index);
    metrics::sms_received(module_id);

    std::string cmd = "AT+CMGR=" + std::to_string(index);
    if (!at.send(cmd)) {
        LOG_ERROR("[%s] failed to send CMGR command", module_id.c_str());
        return false;
    }

    std::string cmgr_header;
    std::string sms_body;
    bool got_header = false;

    auto deadline = std::chrono::steady_clock::now()
        + std::chrono::milliseconds(CMGR_RESPONSE_TIMEOUT_MS);

    while (std::chrono::steady_clock::now() < deadline) {
        auto line = at.poll_urc();
        if (!line) continue;

        if (line->find("+CMGR:") != std::string::npos) {
            cmgr_header = *line;
            got_header = true;
            continue;
        }

        if (got_header && *line != "OK" && line->find("ERROR") == std::string::npos) {
            if (!sms_body.empty()) sms_body += "\n";
            sms_body += *line;
            continue;
        }

        if (*line == "OK" || line->find("ERROR") != std::string::npos) {
            break;
        }
    }

    if (!got_header) {
        LOG_WARN("[%s] no CMGR response for index %d", module_id.c_str(), index);
        return false;
    }

    SmsMessage msg;
    if (!parse_cmgr(cmgr_header, sms_body, msg)) {
        LOG_WARN("[%s] failed to parse CMGR response", module_id.c_str());
        return false;
    }
    msg.module_id = module_id;
    msg.receiver = receiver;

    if (msg.timestamp.empty()) {
        msg.timestamp = utc_now_iso8601();
    }

    SmsRecord record;
    record.sender = msg.sender;
    record.body = msg.body;
    record.received_at = msg.timestamp;
    record.module_id = msg.module_id;

    std::string initial_status = config_.discord_webhook_url.empty() ? "skipped" : "pending";
    record.discord_status = initial_status;

    int64_t record_id = store_.insert(record);
    if (record_id > 0) {
        metrics::sms_db_write(true);
        LOG_INFO("[%s] SMS persisted: id=%ld from=%s",
                 module_id.c_str(), record_id, msg.sender.c_str());
    } else {
        metrics::sms_db_write(false);
        LOG_ERROR("[%s] SMS DB insert failed", module_id.c_str());
    }

    std::string delete_cmd = "AT+CMGD=" + std::to_string(index);
    if (!at.send_and_expect_ok(delete_cmd, 3000)) {
        LOG_WARN("[%s] failed to delete SMS index %d from SIM", module_id.c_str(), index);
    }

    if (!config_.discord_webhook_url.empty() && record_id > 0) {
        std::lock_guard<std::mutex> lock(queue_mutex_);
        queue_.push({msg, record_id});
        queue_cv_.notify_one();
    } else if (config_.discord_webhook_url.empty()) {
        metrics::sms_forwarded(module_id, "skipped");
    }

    return true;
}

std::string SmsHandler::build_discord_payload(const SmsMessage& msg) {
    std::ostringstream json;
    json << R"({"embeds":[{"title":"SMS Received","color":)" << DISCORD_EMBED_COLOR
         << R"(,"fields":[)"
         << R"({"name":"From","value":")" << escape_json(msg.sender) << R"(","inline":true},)";

    if (!msg.receiver.empty()) {
        json << R"({"name":"To","value":")" << escape_json(msg.receiver) << R"(","inline":true},)";
    }

    json << R"({"name":"Module","value":")" << escape_json(msg.module_id) << R"(","inline":true},)"
         << R"({"name":"Time","value":")" << escape_json(msg.timestamp) << R"(","inline":true}],)"
         << R"("description":")" << escape_json(msg.body) << R"("}]})";
    return json.str();
}

void SmsHandler::forward_worker() {
    while (running_.load(std::memory_order_acquire)) {
        ForwardTask task;
        {
            std::unique_lock<std::mutex> lock(queue_mutex_);
            queue_cv_.wait_for(lock, std::chrono::seconds(1), [this] {
                return !queue_.empty() || !running_.load(std::memory_order_acquire);
            });
            if (queue_.empty()) continue;
            task = std::move(queue_.front());
            queue_.pop();
        }
        post_to_discord(task.msg, task.record_id);
    }

    std::lock_guard<std::mutex> lock(queue_mutex_);
    while (!queue_.empty()) {
        auto task = std::move(queue_.front());
        queue_.pop();
        post_to_discord(task.msg, task.record_id);
    }
}

bool SmsHandler::post_to_discord(const SmsMessage& msg, int64_t record_id) {
    auto url = config_.discord_webhook_url;

    auto scheme_end = url.find("://");
    if (scheme_end == std::string::npos) {
        LOG_ERROR("sms_handler: invalid webhook URL (no scheme)");
        store_.update_discord_status(record_id, "failed");
        metrics::sms_forwarded(msg.module_id, "failed");
        return false;
    }

    auto host_start = scheme_end + 3;
    auto path_start = url.find('/', host_start);
    std::string base = url.substr(0, path_start);
    std::string path = (path_start != std::string::npos) ? url.substr(path_start) : "/";

    httplib::Client cli(base);
    cli.set_connection_timeout(DISCORD_TIMEOUT_SEC);
    cli.set_read_timeout(DISCORD_TIMEOUT_SEC);
    cli.set_write_timeout(DISCORD_TIMEOUT_SEC);

    std::string payload = build_discord_payload(msg);
    auto res = cli.Post(path, payload, "application/json");

    if (res && (res->status >= 200 && res->status < 300)) {
        std::string now = utc_now_iso8601();
        store_.update_discord_status(record_id, "sent", now);
        metrics::sms_forwarded(msg.module_id, "sent");
        LOG_INFO("[%s] SMS forwarded to Discord: id=%ld", msg.module_id.c_str(), record_id);
        return true;
    }

    std::string reason = res ? ("HTTP " + std::to_string(res->status)) : "connection_error";
    store_.update_discord_status(record_id, "failed");
    metrics::sms_forwarded(msg.module_id, "failed");
    LOG_WARN("[%s] Discord POST failed: %s (id=%ld)",
             msg.module_id.c_str(), reason.c_str(), record_id);
    return false;
}
