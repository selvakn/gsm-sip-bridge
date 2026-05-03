# Research: SMS to Discord Forwarding

## R1: HTTP Client for Discord Webhook POST

**Decision**: cpp-httplib v0.41.0 (yhirose/cpp-httplib)
**Rationale**:
- MIT license, header-only single file (13k+ GitHub stars)
- C++11 minimum, fully C++17 compatible
- Supports HTTPS POST with OpenSSL (already available for PJSIP)
- Simple synchronous API: `httplib::Client cli("https://discord.com"); cli.Post(path, body, "application/json")`
- CMake-native with FetchContent support

**Alternatives considered**:
- libcurl wrapper (libcpp-http-client): Adds system dependency on libcurl
- Boost.Beast: Heavy dependency, overkill for a single POST request
- Custom socket implementation: Reinventing the wheel, no TLS support

## R2: SQLite Integration

**Decision**: Raw SQLite3 C API via system package (`libsqlite3-dev`)
**Rationale**:
- Public domain license (no restrictions)
- Available in Debian bookworm as `libsqlite3-dev` (builder) and `libsqlite3-0` (runtime)
- Simple use case (single table, insert + update) doesn't warrant a C++ wrapper
- Extremely well-tested and stable
- Thread-safe in serialized mode (`SQLITE_THREADSAFE=1`, default)

**Alternatives considered**:
- BetterSql (header-only C++17 wrapper): Unnecessary abstraction for single-table use
- SQLiteCpp: Another wrapper, adds build complexity for minimal benefit
- FetchContent for sqlite3 amalgamation: Possible but system package is simpler

## R3: EC20 SMS AT Command Protocol

**Decision**: Use `+CMTI` URC mode (store-then-read) with text mode
**Rationale**:
- `AT+CMGF=1` enables text mode (human-readable sender/body)
- `AT+CNMI=2,1,0,0,0` configures the module to send `+CMTI: "SM",<index>` when SMS arrives
- Read with `AT+CMGR=<index>` to get sender, timestamp, body
- Delete with `AT+CMGD=<index>` after successful DB write
- Safer than direct delivery (`+CMT` with mt=2) because message is persisted on SIM until we explicitly delete
- Fits naturally into existing `poll_urc()` loop

**Protocol flow**:
1. Initialization: `AT+CMGF=1` → `AT+CNMI=2,1,0,0,0`
2. URC arrives: `+CMTI: "SM",3`
3. Read: `AT+CMGR=3` → `+CMGR: "REC UNREAD","+1234567890","","2026/05/03,14:30:00+20"\r\nMessage body\r\nOK`
4. Write to SQLite
5. POST to Discord (async)
6. Delete: `AT+CMGD=3`

**Multi-part SMS**: In text mode, the EC20 stores concatenated SMS as separate entries. For simplicity, each part is forwarded individually. Full concatenation deferred to a future enhancement.

## R4: Asynchronous Discord Posting

**Decision**: Background worker thread with a message queue
**Rationale**:
- SMS is infrequent (assumption from spec), so a simple queue + single worker thread is sufficient
- AT command loop pushes SMS to queue without blocking
- Worker thread pops and performs synchronous HTTP POST via cpp-httplib
- On failure, logs warning; no retry (per spec, SMS is already in DB)

**Alternatives considered**:
- `std::async` per SMS: Creates unbounded threads under load
- Detached threads per SMS: Same issue, harder to join on shutdown
- Thread pool: Over-engineered for infrequent events

## R5: Discord Webhook URL Parsing

**Decision**: Parse the full webhook URL from config, extract host and path for cpp-httplib
**Rationale**:
- Discord webhook URLs follow format: `https://discord.com/api/webhooks/<id>/<token>`
- cpp-httplib requires separate host and path
- Simple string parsing; no URL library needed
