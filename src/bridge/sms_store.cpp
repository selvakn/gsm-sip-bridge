#include "bridge/sms_store.h"
#include "logger.h"

#include <sqlite3.h>

static constexpr const char* SCHEMA_SQL = R"(
CREATE TABLE IF NOT EXISTS sms (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    sender         TEXT    NOT NULL,
    body           TEXT    NOT NULL,
    received_at    TEXT    NOT NULL,
    module_id      TEXT    NOT NULL,
    discord_status TEXT    NOT NULL DEFAULT 'pending',
    forwarded_at   TEXT
);
CREATE INDEX IF NOT EXISTS idx_sms_received_at ON sms(received_at);
CREATE INDEX IF NOT EXISTS idx_sms_module_id ON sms(module_id);
)";

SmsStore::~SmsStore() {
    close();
}

bool SmsStore::open(const std::string& db_path) {
    if (db_) return true;

    int rc = sqlite3_open(db_path.c_str(), &db_);
    if (rc != SQLITE_OK) {
        LOG_ERROR("sms_store: cannot open database %s: %s",
                  db_path.c_str(), sqlite3_errmsg(db_));
        sqlite3_close(db_);
        db_ = nullptr;
        return false;
    }

    sqlite3_exec(db_, "PRAGMA journal_mode=WAL;", nullptr, nullptr, nullptr);
    sqlite3_exec(db_, "PRAGMA synchronous=NORMAL;", nullptr, nullptr, nullptr);

    if (!create_schema()) {
        close();
        return false;
    }

    LOG_INFO("sms_store: opened %s", db_path.c_str());
    return true;
}

void SmsStore::close() {
    if (db_) {
        sqlite3_close(db_);
        db_ = nullptr;
    }
}

bool SmsStore::create_schema() {
    char* errmsg = nullptr;
    int rc = sqlite3_exec(db_, SCHEMA_SQL, nullptr, nullptr, &errmsg);
    if (rc != SQLITE_OK) {
        LOG_ERROR("sms_store: schema creation failed: %s", errmsg ? errmsg : "unknown");
        sqlite3_free(errmsg);
        return false;
    }
    return true;
}

int64_t SmsStore::insert(const SmsRecord& record) {
    if (!db_) return -1;

    static constexpr const char* SQL =
        "INSERT INTO sms (sender, body, received_at, module_id, discord_status) "
        "VALUES (?, ?, ?, ?, ?)";

    sqlite3_stmt* stmt = nullptr;
    int rc = sqlite3_prepare_v2(db_, SQL, -1, &stmt, nullptr);
    if (rc != SQLITE_OK) {
        LOG_ERROR("sms_store: insert prepare failed: %s", sqlite3_errmsg(db_));
        return -1;
    }

    sqlite3_bind_text(stmt, 1, record.sender.c_str(), -1, SQLITE_TRANSIENT);
    sqlite3_bind_text(stmt, 2, record.body.c_str(), -1, SQLITE_TRANSIENT);
    sqlite3_bind_text(stmt, 3, record.received_at.c_str(), -1, SQLITE_TRANSIENT);
    sqlite3_bind_text(stmt, 4, record.module_id.c_str(), -1, SQLITE_TRANSIENT);

    std::string status = record.discord_status.empty() ? "pending" : record.discord_status;
    sqlite3_bind_text(stmt, 5, status.c_str(), -1, SQLITE_TRANSIENT);

    rc = sqlite3_step(stmt);
    int64_t row_id = -1;
    if (rc == SQLITE_DONE) {
        row_id = sqlite3_last_insert_rowid(db_);
    } else {
        LOG_ERROR("sms_store: insert failed: %s", sqlite3_errmsg(db_));
    }

    sqlite3_finalize(stmt);
    return row_id;
}

bool SmsStore::update_discord_status(int64_t id, const std::string& status,
                                     const std::string& forwarded_at) {
    if (!db_) return false;

    static constexpr const char* SQL =
        "UPDATE sms SET discord_status = ?, forwarded_at = ? WHERE id = ?";

    sqlite3_stmt* stmt = nullptr;
    int rc = sqlite3_prepare_v2(db_, SQL, -1, &stmt, nullptr);
    if (rc != SQLITE_OK) {
        LOG_ERROR("sms_store: update prepare failed: %s", sqlite3_errmsg(db_));
        return false;
    }

    sqlite3_bind_text(stmt, 1, status.c_str(), -1, SQLITE_TRANSIENT);
    if (forwarded_at.empty()) {
        sqlite3_bind_null(stmt, 2);
    } else {
        sqlite3_bind_text(stmt, 2, forwarded_at.c_str(), -1, SQLITE_TRANSIENT);
    }
    sqlite3_bind_int64(stmt, 3, id);

    rc = sqlite3_step(stmt);
    bool ok = (rc == SQLITE_DONE);
    if (!ok) {
        LOG_ERROR("sms_store: update failed: %s", sqlite3_errmsg(db_));
    }

    sqlite3_finalize(stmt);
    return ok;
}

std::vector<SmsRecord> SmsStore::fetch_pending(int limit) {
    std::vector<SmsRecord> results;
    if (!db_) return results;

    static constexpr const char* SQL =
        "SELECT id, sender, body, received_at, module_id, discord_status, forwarded_at "
        "FROM sms WHERE discord_status = 'pending' ORDER BY id ASC LIMIT ?";

    sqlite3_stmt* stmt = nullptr;
    int rc = sqlite3_prepare_v2(db_, SQL, -1, &stmt, nullptr);
    if (rc != SQLITE_OK) return results;

    sqlite3_bind_int(stmt, 1, limit);

    while (sqlite3_step(stmt) == SQLITE_ROW) {
        SmsRecord rec;
        rec.id = sqlite3_column_int64(stmt, 0);
        rec.sender = reinterpret_cast<const char*>(sqlite3_column_text(stmt, 1));
        rec.body = reinterpret_cast<const char*>(sqlite3_column_text(stmt, 2));
        rec.received_at = reinterpret_cast<const char*>(sqlite3_column_text(stmt, 3));
        rec.module_id = reinterpret_cast<const char*>(sqlite3_column_text(stmt, 4));
        rec.discord_status = reinterpret_cast<const char*>(sqlite3_column_text(stmt, 5));

        const char* fwd = reinterpret_cast<const char*>(sqlite3_column_text(stmt, 6));
        if (fwd) rec.forwarded_at = fwd;

        results.push_back(std::move(rec));
    }

    sqlite3_finalize(stmt);
    return results;
}

int64_t SmsStore::count() {
    if (!db_) return 0;

    static constexpr const char* SQL = "SELECT COUNT(*) FROM sms";
    sqlite3_stmt* stmt = nullptr;
    int rc = sqlite3_prepare_v2(db_, SQL, -1, &stmt, nullptr);
    if (rc != SQLITE_OK) return 0;

    int64_t result = 0;
    if (sqlite3_step(stmt) == SQLITE_ROW) {
        result = sqlite3_column_int64(stmt, 0);
    }

    sqlite3_finalize(stmt);
    return result;
}
