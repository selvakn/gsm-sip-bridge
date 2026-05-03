#pragma once

#include <cstdint>
#include <string>
#include <vector>

struct sqlite3;

struct SmsRecord {
    int64_t id = 0;
    std::string sender;
    std::string body;
    std::string received_at;
    std::string module_id;
    std::string discord_status = "pending";
    std::string forwarded_at;
};

class SmsStore {
public:
    SmsStore() = default;
    ~SmsStore();

    SmsStore(const SmsStore&) = delete;
    SmsStore& operator=(const SmsStore&) = delete;

    bool open(const std::string& db_path);
    void close();

    int64_t insert(const SmsRecord& record);
    bool update_discord_status(int64_t id, const std::string& status,
                               const std::string& forwarded_at = "");

    std::vector<SmsRecord> fetch_pending(int limit = 100);
    int64_t count();

private:
    sqlite3* db_ = nullptr;

    bool create_schema();
};
