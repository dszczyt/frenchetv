// Minimal C++ CDM host shim for Widevine CDM interface version 10.
//
// Defines its own CDM interface types (no Google headers needed) matching
// the Itanium C++ ABI that libwidevinecdm.so was compiled with.
// Exposes a plain C API to Rust.
//
// Thread safety: CDM callbacks are called synchronously from within
// CreateSessionAndGenerateRequest / UpdateSession on the calling thread.

#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <dlfcn.h>
#include <stdio.h>
#include <vector>

// ─── CDM types (matching chromium CDM interface 10 ABI) ───────────────────────

namespace cdm {

enum class Exception : uint32_t {
    kExceptionTypeError          = 0,
    kExceptionNotSupportedError  = 1,
    kExceptionInvalidStateError  = 2,
    kExceptionQuotaExceededError = 3,
};

enum class KeyStatus : uint32_t {
    kUsable            = 0,
    kInternalError     = 1,
    kExpired           = 2,
    kOutputRestricted  = 3,
    kOutputDownscaled  = 4,
    kStatusPending     = 5,
    kReleased          = 6,
};

enum class SessionType : uint32_t {
    kTemporary            = 0,
    kPersistentLicense    = 1,
    kPersistentKeyRelease = 2,
};

enum class InitDataType : uint32_t {
    kCenc  = 0,
    kKeyIds = 1,
    kWebM  = 2,
};

enum class MessageType : uint32_t {
    kLicenseRequest          = 0,
    kLicenseRenewal          = 1,
    kLicenseRelease          = 2,
    kIndividualizationRequest = 3,
};

enum class EncryptionScheme : uint32_t {
    kUnencrypted = 0,
    kCenc        = 1,
    kCbcs        = 2,
};

enum Status : uint32_t {
    kSuccess               = 0,
    kNoKey                 = 1,
    kNeedMoreData          = 2,
    kDecryptError          = 3,
    kDecodeError           = 4,
    kInitializationError   = 5,
    kDeferredInitialization = 6,
};

struct SubsampleEntry {
    uint32_t clear_bytes;
    uint32_t cipher_bytes;
};

struct InputBuffer_2 {
    const uint8_t*      data;
    uint32_t            data_size;
    EncryptionScheme    encryption_scheme;
    const uint8_t*      key_id;
    uint32_t            key_id_size;
    const uint8_t*      iv;
    uint32_t            iv_size;
    const SubsampleEntry* subsamples;
    uint32_t            num_subsamples;
    int64_t             timestamp;
    const uint8_t*      side_data;
    uint32_t            side_data_size;
};

struct KeyInformation {
    const uint8_t* key_id;
    uint32_t       key_id_size;
    KeyStatus      status;
    uint32_t       system_code;
};

struct Policy {
    uint32_t min_hdcp_version;
};

// Forward declarations
class FileIO;
class FileIOClient;

// ─── Buffer ─────────────────────────────────────────────────────────────────

class Buffer {
 public:
    virtual void     Destroy()         = 0;
    virtual uint32_t Capacity() const  = 0;
    virtual uint8_t* Data()            = 0;
    virtual void     SetSize(uint32_t) = 0;
    virtual uint32_t Size()    const   = 0;
 protected:
    Buffer() {}
    virtual ~Buffer() {}
};

class SimpleBuffer : public Buffer {
 public:
    explicit SimpleBuffer(uint32_t cap)
        : data_(static_cast<uint8_t*>(malloc(cap))), cap_(cap), size_(0) {}
    void     Destroy()              override { delete this; }
    uint32_t Capacity() const       override { return cap_; }
    uint8_t* Data()                 override { return data_; }
    void     SetSize(uint32_t s)    override { size_ = s; }
    uint32_t Size()    const        override { return size_; }
 private:
    ~SimpleBuffer() { free(data_); }
    uint8_t* data_;
    uint32_t cap_;
    uint32_t size_;
};

// ─── DecryptedBlock ──────────────────────────────────────────────────────────

class DecryptedBlock {
 public:
    virtual void    SetDecryptedBuffer(Buffer*)  = 0;
    virtual Buffer* DecryptedBuffer()            = 0;
    virtual void    SetTimestamp(int64_t)        = 0;
    virtual int64_t Timestamp() const            = 0;
 protected:
    DecryptedBlock() {}
    virtual ~DecryptedBlock() {}
};

class SimpleDecryptedBlock : public DecryptedBlock {
 public:
    SimpleDecryptedBlock() : buf_(nullptr), ts_(0) {}
    ~SimpleDecryptedBlock() { if (buf_) buf_->Destroy(); }
    void    SetDecryptedBuffer(Buffer* b) override { buf_ = b; }
    Buffer* DecryptedBuffer()             override { return buf_; }
    void    SetTimestamp(int64_t t)       override { ts_ = t; }
    int64_t Timestamp() const             override { return ts_; }
 private:
    Buffer* buf_;
    int64_t ts_;
};

// ─── Host_10 (pure virtual interface we must implement) ──────────────────────

class Host_10 {
 public:
    static const int kVersion = 10;
    virtual Buffer* Allocate(uint32_t capacity)                               = 0;
    virtual void    SetTimer(int64_t delay_ms, void* context)                 = 0;
    virtual double  GetCurrentWallTime()                                      = 0;
    virtual void    OnInitialized(bool success)                               = 0;
    virtual void    OnResolveKeyStatusPromise(uint32_t, KeyStatus)            = 0;
    virtual void    OnResolveNewSessionPromise(uint32_t, const char*, uint32_t) = 0;
    virtual void    OnResolvePromise(uint32_t)                                = 0;
    virtual void    OnRejectPromise(uint32_t, Exception, uint32_t,
                                    const char*, uint32_t)                    = 0;
    virtual void    OnSessionMessage(const char*, uint32_t, MessageType,
                                     const char*, uint32_t)                   = 0;
    virtual void    OnSessionKeysChange(const char*, uint32_t, bool,
                                        const KeyInformation*, uint32_t)      = 0;
    virtual void    OnExpirationChange(const char*, uint32_t, double)         = 0;
    virtual void    OnSessionClosed(const char*, uint32_t)                    = 0;
    virtual void    SendPlatformChallenge(const char*, uint32_t,
                                          const char*, uint32_t)              = 0;
    virtual void    EnableOutputProtection(uint32_t)                          = 0;
    virtual void    QueryOutputProtectionStatus()                             = 0;
    virtual void    OnDeferredInitializationDone(uint32_t, Status)            = 0;
    virtual FileIO* CreateFileIO(FileIOClient*)                               = 0;
    virtual void    RequestStorageId(uint32_t)                                = 0;
 protected:
    Host_10() {}
    virtual ~Host_10() {}
};

// ─── ContentDecryptionModule_10 (vtable we call INTO the CDM) ────────────────

class ContentDecryptionModule_10 {
 public:
    static const int kVersion = 10;
    virtual void   Initialize(bool allow_distinctive_identifier,
                              bool allow_persistent_state,
                              bool use_hw_secure_codecs)                         = 0;
    virtual void   GetStatusForPolicy(uint32_t promise_id, const Policy& policy) = 0;
    virtual void   SetServerCertificate(uint32_t, const uint8_t*, uint32_t)      = 0;
    virtual void   CreateSessionAndGenerateRequest(uint32_t, SessionType,
                                                   InitDataType,
                                                   const uint8_t*, uint32_t)     = 0;
    virtual void   LoadSession(uint32_t, SessionType, const char*, uint32_t)     = 0;
    virtual void   UpdateSession(uint32_t, const char*, uint32_t,
                                 const uint8_t*, uint32_t)                       = 0;
    virtual void   CloseSession(uint32_t, const char*, uint32_t)                 = 0;
    virtual void   RemoveSession(uint32_t, const char*, uint32_t)                = 0;
    virtual void   TimerExpired(void* context)                                   = 0;
    virtual Status Decrypt(const InputBuffer_2& encrypted_buffer,
                           DecryptedBlock* decrypted_buffer)                     = 0;
    virtual Status InitializeAudioDecoder(const void*)                           = 0;
    virtual Status InitializeVideoDecoder(const void*)                           = 0;
    virtual Status DecryptAndDecodeFrame(const InputBuffer_2&, void*)            = 0;
    virtual Status DecryptAndDecodeSamples(const InputBuffer_2&, void*)          = 0;
    virtual void   OnPlatformChallengeResponse(const void*)                      = 0;
    virtual void   OnQueryOutputProtectionStatus(uint32_t, uint32_t, uint32_t)  = 0;
    virtual void   OnStorageId(uint32_t, const uint8_t*, uint32_t)               = 0;
    virtual void   Destroy()                                                     = 0;
 protected:
    ContentDecryptionModule_10() {}
    virtual ~ContentDecryptionModule_10() {}
};

} // namespace cdm

// ─── Exported CDM function types ─────────────────────────────────────────────

typedef void (*InitCdmFn)();
typedef void (*DeinitCdmFn)();
typedef cdm::Host_10* (*GetHostFn)(int, void*);
typedef cdm::ContentDecryptionModule_10* (*CreateInstanceFn)(
    int, const char*, uint32_t, GetHostFn, void*);

// ─── C API types (Rust-facing) ───────────────────────────────────────────────

extern "C" {

struct CdmCallbacks {
    void (*on_initialized)(void* ctx, bool success);
    // License request is ready — POST these bytes to the license server.
    void (*on_license_request)(void* ctx,
                               const char* session_id, uint32_t session_id_len,
                               const uint8_t* request, uint32_t request_len);
    // Keys status changed.
    void (*on_keys_change)(void* ctx,
                           const char* session_id, uint32_t session_id_len,
                           bool has_usable_key);
    // Promise resolved (session created or update accepted).
    void (*on_promise_ok)(void* ctx, uint32_t promise_id,
                          const char* session_id, uint32_t session_id_len);
    // Promise rejected.
    void (*on_promise_err)(void* ctx, uint32_t promise_id,
                           const char* msg, uint32_t msg_len);
    void* ctx;
};

struct CdmDecryptInput {
    const uint8_t*             data;
    uint32_t                   data_size;
    uint32_t                   encryption_scheme; // 1=cenc, 2=cbcs
    const uint8_t*             key_id;
    uint32_t                   key_id_size;
    const uint8_t*             iv;
    uint32_t                   iv_size;
    const cdm::SubsampleEntry* subsamples;
    uint32_t                   num_subsamples;
    int64_t                    timestamp;
};

struct CdmDecryptOutput {
    uint8_t* data;      // caller must free via cdm_free_output()
    uint32_t data_size;
    int      status;    // 0 = kSuccess
};

} // extern "C"

// ─── Host implementation ─────────────────────────────────────────────────────

// Forward declaration so CdmHostImpl can store loaded key IDs in CdmState.
struct CdmState;

class CdmHostImpl : public cdm::Host_10 {
 public:
    explicit CdmHostImpl(CdmCallbacks* cbs, CdmState* state) : cbs_(cbs), state_(state) {}
    ~CdmHostImpl() override {}

    cdm::Buffer* Allocate(uint32_t capacity) override {
        return new cdm::SimpleBuffer(capacity);
    }
    void SetTimer(int64_t delay_ms, void* context) override;  // defined after CdmState

    double GetCurrentWallTime() override {
        struct timespec ts;
        clock_gettime(CLOCK_REALTIME, &ts);
        return static_cast<double>(ts.tv_sec)
             + static_cast<double>(ts.tv_nsec) / 1.0e9;
    }

    void OnInitialized(bool success) override {
        if (cbs_->on_initialized)
            cbs_->on_initialized(cbs_->ctx, success);
    }

    void OnResolveKeyStatusPromise(uint32_t promise_id, cdm::KeyStatus status) override {
        fprintf(stderr, "[CDM] OnResolveKeyStatusPromise(promise=%u, status=%u)\n",
                promise_id, static_cast<unsigned>(status));
        fflush(stderr);
    }

    void OnResolveNewSessionPromise(uint32_t promise_id,
                                    const char* session_id,
                                    uint32_t session_id_size) override {
        if (cbs_->on_promise_ok)
            cbs_->on_promise_ok(cbs_->ctx, promise_id, session_id, session_id_size);
    }

    void OnResolvePromise(uint32_t promise_id) override {
        if (cbs_->on_promise_ok)
            cbs_->on_promise_ok(cbs_->ctx, promise_id, nullptr, 0);
    }

    void OnRejectPromise(uint32_t promise_id, cdm::Exception /*ex*/, uint32_t,
                         const char* msg, uint32_t msg_len) override {
        if (cbs_->on_promise_err)
            cbs_->on_promise_err(cbs_->ctx, promise_id, msg, msg_len);
    }

    void OnSessionMessage(const char* session_id, uint32_t session_id_size,
                          cdm::MessageType message_type,
                          const char* message, uint32_t message_size) override {
        if ((message_type == cdm::MessageType::kLicenseRequest ||
             message_type == cdm::MessageType::kLicenseRenewal) && cbs_->on_license_request) {
            cbs_->on_license_request(cbs_->ctx,
                                     session_id, session_id_size,
                                     reinterpret_cast<const uint8_t*>(message),
                                     message_size);
        }
    }

    void OnSessionKeysChange(const char* session_id, uint32_t session_id_size,
                             bool has_additional_usable_key,
                             const cdm::KeyInformation* keys_info,
                             uint32_t keys_info_count) override;  // defined after CdmState

    void OnExpirationChange(const char*, uint32_t, double expiry) override {
        fprintf(stderr, "[CDM] OnExpirationChange(expiry=%f)\n", expiry);
        fflush(stderr);
    }
    void OnSessionClosed(const char*, uint32_t) override {
        fprintf(stderr, "[CDM] OnSessionClosed\n");
        fflush(stderr);
    }
    void SendPlatformChallenge(const char*, uint32_t, const char*, uint32_t) override {
        fprintf(stderr, "[CDM] SendPlatformChallenge (no-op)\n");
        fflush(stderr);
    }
    void EnableOutputProtection(uint32_t protection_mask) override {
        fprintf(stderr, "[CDM] EnableOutputProtection(mask=0x%x)\n", protection_mask);
        fflush(stderr);
    }
    void QueryOutputProtectionStatus() override;  // defined after CdmState (needs complete type)
    void OnDeferredInitializationDone(uint32_t stream_type, cdm::Status decode_status) override {
        fprintf(stderr, "[CDM] OnDeferredInitializationDone(stream=%u, status=%u)\n",
                stream_type, static_cast<unsigned>(decode_status));
        fflush(stderr);
    }
    cdm::FileIO* CreateFileIO(cdm::FileIOClient*) override {
        fprintf(stderr, "[CDM] CreateFileIO (returning nullptr)\n");
        fflush(stderr);
        return nullptr;
    }
    void RequestStorageId(uint32_t version) override;  // defined after CdmState

 private:
    CdmCallbacks* cbs_;
    CdmState*     state_;
};

// ─── CDM state ───────────────────────────────────────────────────────────────

struct CdmState {
    void*                              lib_handle;
    DeinitCdmFn                        deinit;
    cdm::ContentDecryptionModule_10*   cdm;
    CdmHostImpl*                       host;
    CdmCallbacks                       callbacks; // owned copy
    // Loaded key IDs + statuses (populated in OnSessionKeysChange).
    std::vector<std::vector<uint8_t>>  loaded_key_ids;
    std::vector<uint32_t>              loaded_key_statuses; // parallel to loaded_key_ids
    // Set by QueryOutputProtectionStatus() host callback; cleared by answer_ops_query().
    bool                               output_protection_query_pending = false;
    // Contexts registered via SetTimer(); fired by fire_pending_timers() outside CDM call stack.
    std::vector<void*>                 pending_timer_contexts;
    // Set by RequestStorageId(); cleared by answer_storage_id().
    bool                               storage_id_requested = false;
    uint32_t                           storage_id_version = 0;
};

// Respond to a pending QueryOutputProtectionStatus request.
// MUST be called from outside any CDM call stack to avoid re-entrancy.
// Reports: internal display present, HDCP active, query succeeded.
static void answer_ops_query(CdmState* state) {
    if (!state || !state->output_protection_query_pending || !state->cdm) return;
    state->output_protection_query_pending = false;
    fprintf(stderr, "[CDM] answer_ops_query: OnQueryOutputProtectionStatus(link=1, HDCP, ok)\n");
    fflush(stderr);
    state->cdm->OnQueryOutputProtectionStatus(
        /*link_mask=*/1,
        /*output_protection_mask=*/8,   // kProtectionHDCP
        /*result=*/0);                  // kQuerySucceeded
}

// Fire all pending SetTimer() callbacks (safely, outside any CDM call stack),
// then answer any OPS query that the timers may have triggered.
// The CDM uses SetTimer to schedule deferred policy re-checks; if we never fire
// them it can return kNeedMoreData from Decrypt.
static void fire_pending_timers(CdmState* state) {
    if (!state || !state->cdm || state->pending_timer_contexts.empty()) return;
    std::vector<void*> contexts;
    std::swap(contexts, state->pending_timer_contexts); // consume to avoid infinite loop
    fprintf(stderr, "[CDM] fire_pending_timers: %zu timer(s)\n", contexts.size());
    fflush(stderr);
    for (void* ctx : contexts) {
        state->cdm->TimerExpired(ctx);
    }
    // Timers may have triggered an OPS query — answer it now (outside CDM call stack).
    answer_ops_query(state);
}

// Respond to a pending RequestStorageId() call with an empty storage ID.
// MUST be called outside any CDM call stack to avoid re-entrancy.
static void answer_storage_id(CdmState* state) {
    if (!state || !state->storage_id_requested || !state->cdm) return;
    uint32_t version = state->storage_id_version;
    state->storage_id_requested = false;
    fprintf(stderr, "[CDM] answer_storage_id: OnStorageId(version=%u, empty)\n", version);
    fflush(stderr);
    state->cdm->OnStorageId(version, nullptr, 0);
}

// Out-of-line definition of RequestStorageId (needs complete CdmState type).
void CdmHostImpl::RequestStorageId(uint32_t version) {
    fprintf(stderr, "[CDM] RequestStorageId(version=%u) — deferring response\n", version);
    fflush(stderr);
    if (state_) {
        state_->storage_id_requested = true;
        state_->storage_id_version   = version;
    }
}

// Out-of-line definition of SetTimer (needs complete CdmState type).
void CdmHostImpl::SetTimer(int64_t delay_ms, void* context) {
    fprintf(stderr, "[CDM] SetTimer(delay=%lld ms, ctx=%p)\n", (long long)delay_ms, context);
    fflush(stderr);
    if (state_ && context) state_->pending_timer_contexts.push_back(context);
}

// Out-of-line definition of QueryOutputProtectionStatus (needs complete CdmState type).
void CdmHostImpl::QueryOutputProtectionStatus() {
    // Mark the query as pending; answer_ops_query() will respond after the
    // current CDM call returns (avoiding re-entrancy).
    if (state_) state_->output_protection_query_pending = true;
    fprintf(stderr, "[CDM] QueryOutputProtectionStatus — pending flag set\n");
    fflush(stderr);
}

// Out-of-line definition of OnSessionKeysChange (needs complete CdmState type).
void CdmHostImpl::OnSessionKeysChange(const char* session_id, uint32_t session_id_size,
                                       bool has_additional_usable_key,
                                       const cdm::KeyInformation* keys_info,
                                       uint32_t keys_info_count) {
    bool any_usable = has_additional_usable_key;
    // Store all key IDs + statuses so cdm_decrypt can log them on failure.
    if (state_) { state_->loaded_key_ids.clear(); state_->loaded_key_statuses.clear(); }
    for (uint32_t i = 0; i < keys_info_count; ++i) {
        if (keys_info[i].status == cdm::KeyStatus::kUsable) any_usable = true;
        if (state_) {
            state_->loaded_key_ids.push_back(std::vector<uint8_t>(
                keys_info[i].key_id,
                keys_info[i].key_id + keys_info[i].key_id_size));
            state_->loaded_key_statuses.push_back(
                static_cast<uint32_t>(keys_info[i].status));
        }
        fprintf(stderr, "[CDM] OnSessionKeysChange key[%u] status=%u id=", i,
                static_cast<unsigned>(keys_info[i].status));
        for (uint32_t j = 0; j < keys_info[i].key_id_size; ++j)
            fprintf(stderr, "%02x", keys_info[i].key_id[j]);
        fprintf(stderr, "\n");
    }
    fflush(stderr);
    if (cbs_->on_keys_change)
        cbs_->on_keys_change(cbs_->ctx, session_id, session_id_size, any_usable);
}

// ─── C API ───────────────────────────────────────────────────────────────────

static cdm::Host_10* get_host_fn(int version, void* user_data) {
    if (version == cdm::Host_10::kVersion)
        return static_cast<CdmHostImpl*>(user_data);
    return nullptr;
}

extern "C" {

/// Load libwidevinecdm.so, initialise module, create CDM instance.
/// Returns null on failure.
CdmState* cdm_create(const char* lib_path, const CdmCallbacks* callbacks) {
    void* handle = dlopen(lib_path, RTLD_LAZY | RTLD_LOCAL);
    if (!handle) {
        fprintf(stderr, "cdm_create: dlopen(%s): %s\n", lib_path, dlerror());
        return nullptr;
    }

    auto init_fn   = reinterpret_cast<InitCdmFn>  (dlsym(handle, "InitializeCdmModule_4"));
    auto deinit_fn = reinterpret_cast<DeinitCdmFn> (dlsym(handle, "DeinitializeCdmModule"));
    auto create_fn = reinterpret_cast<CreateInstanceFn>(dlsym(handle, "CreateCdmInstance"));

    if (!init_fn || !create_fn) {
        fprintf(stderr, "cdm_create: missing CDM symbols\n");
        dlclose(handle);
        return nullptr;
    }

    init_fn();

    auto* state = new CdmState{};
    state->lib_handle = handle;
    state->deinit     = deinit_fn;
    state->callbacks  = *callbacks;
    state->host       = new CdmHostImpl(&state->callbacks, state);

    static const char key_system[] = "com.widevine.alpha";
    state->cdm = create_fn(cdm::ContentDecryptionModule_10::kVersion,
                            key_system,
                            static_cast<uint32_t>(sizeof(key_system) - 1),
                            get_host_fn,
                            state->host);

    if (!state->cdm) {
        fprintf(stderr, "cdm_create: CreateCdmInstance(10,...) returned null\n");
        delete state->host;
        delete state;
        if (deinit_fn) deinit_fn();
        dlclose(handle);
        return nullptr;
    }

    return state;
}

/// Call Initialize on the CDM instance.
void cdm_initialize(CdmState* state) {
    if (!state || !state->cdm) return;
    state->cdm->Initialize(/*allow_distinctive_identifier=*/true,
                           /*allow_persistent_state=*/false,
                           /*use_hw_secure_codecs=*/false);
    // Respond to any deferred callbacks from Initialize().
    answer_storage_id(state);
    fire_pending_timers(state);
    answer_ops_query(state);
}

/// Start a temporary session with the given PSSH data.
/// Synchronously calls back on_license_request with the license challenge.
void cdm_create_session(CdmState* state, uint32_t promise_id,
                         const uint8_t* pssh, uint32_t pssh_len) {
    if (state && state->cdm)
        state->cdm->CreateSessionAndGenerateRequest(
            promise_id,
            cdm::SessionType::kTemporary,
            cdm::InitDataType::kCenc,
            pssh, pssh_len);
}

/// Feed the license server response into the CDM.
/// On success calls back on_keys_change with has_usable_key=true.
void cdm_update_session(CdmState* state, uint32_t promise_id,
                         const char* session_id, uint32_t session_id_len,
                         const uint8_t* response, uint32_t response_len) {
    if (!state || !state->cdm) return;
    state->cdm->UpdateSession(promise_id,
                               session_id, session_id_len,
                               response, response_len);
    // Respond to any deferred callbacks from UpdateSession().
    answer_storage_id(state);
    fire_pending_timers(state);
    answer_ops_query(state);
}

/// Decrypt one MP4 sample.
///
/// Handles periodic CDM QueryOutputProtectionStatus() re-queries:
/// - Answer any pending OPS query before each attempt (flag set from host callback).
/// - If Decrypt returns kNeedMoreData, the CDM queried OPS *during* the call;
///   answer the new pending query (safely, after Decrypt returned) and retry once.
CdmDecryptOutput cdm_decrypt(CdmState* state, const CdmDecryptInput* inp) {
    CdmDecryptOutput out{nullptr, 0, static_cast<int>(cdm::Status::kDecryptError)};
    if (!state || !state->cdm || !inp) return out;

    cdm::InputBuffer_2 buf{};
    buf.data              = inp->data;
    buf.data_size         = inp->data_size;
    buf.encryption_scheme = static_cast<cdm::EncryptionScheme>(inp->encryption_scheme);
    buf.key_id            = inp->key_id;
    buf.key_id_size       = inp->key_id_size;
    buf.iv                = inp->iv;
    buf.iv_size           = inp->iv_size;
    buf.subsamples        = inp->subsamples;
    buf.num_subsamples    = inp->num_subsamples;
    buf.timestamp         = inp->timestamp;

    cdm::Status status = cdm::Status::kDecryptError;
    for (int attempt = 0; attempt < 2; ++attempt) {
        // Respond to any deferred callbacks, then call Decrypt.
        answer_storage_id(state);
        fire_pending_timers(state);
        answer_ops_query(state);

        fprintf(stderr, "[CDM] Decrypt(attempt=%d, data=%u, subsamples=%u)\n",
                attempt + 1, inp->data_size, inp->num_subsamples);
        fflush(stderr);

        cdm::SimpleDecryptedBlock block;
        status = state->cdm->Decrypt(buf, &block);

        if (status == cdm::Status::kSuccess && block.DecryptedBuffer()) {
            cdm::Buffer* b = block.DecryptedBuffer();
            out.data_size  = b->Size();
            out.data       = static_cast<uint8_t*>(malloc(out.data_size));
            if (out.data) {
                memcpy(out.data, b->Data(), out.data_size);
                out.status = 0;
                fprintf(stderr, "[CDM] Decrypt SUCCESS (attempt=%d, out=%u bytes)\n",
                        attempt + 1, out.data_size);
                fflush(stderr);
            } else {
                out.status = static_cast<int>(cdm::Status::kDecryptError);
            }
            return out;
        }

        if (status != cdm::Status::kNeedMoreData) break; // fatal error, no retry

        // kNeedMoreData: CDM may have issued a new QueryOutputProtectionStatus
        // call during Decrypt(). answer_ops_query() at the top of the next
        // iteration will respond (safely, outside the CDM call stack).
        fprintf(stderr, "[CDM] Decrypt kNeedMoreData (attempt %d, OPS pending=%s)\n",
                attempt + 1, state->output_protection_query_pending ? "yes" : "no");
        fflush(stderr);
    }

    // Both attempts failed — log diagnostics.
    out.status = static_cast<int>(status);
    fprintf(stderr, "[CDM] Decrypt FAILED status=%u scheme=%u data_size=%u\n",
            static_cast<unsigned>(status),
            static_cast<unsigned>(inp->encryption_scheme),
            inp->data_size);
    fprintf(stderr, "[CDM]   requested_kid=");
    for (uint32_t i = 0; i < inp->key_id_size; ++i)
        fprintf(stderr, "%02x", inp->key_id[i]);
    fprintf(stderr, "\n");
    fprintf(stderr, "[CDM]   iv(%u)=", inp->iv_size);
    for (uint32_t i = 0; i < inp->iv_size; ++i)
        fprintf(stderr, "%02x", inp->iv[i]);
    fprintf(stderr, "\n");
    fprintf(stderr, "[CDM]   subsamples(%u):", inp->num_subsamples);
    uint64_t total_sub = 0;
    for (uint32_t i = 0; i < inp->num_subsamples && i < 8; ++i) {
        fprintf(stderr, " [clr=%u enc=%u]",
                inp->subsamples[i].clear_bytes,
                inp->subsamples[i].cipher_bytes);
        total_sub += inp->subsamples[i].clear_bytes + inp->subsamples[i].cipher_bytes;
    }
    if (inp->num_subsamples > 0)
        fprintf(stderr, " total_sub=%llu data=%u %s",
                (unsigned long long)total_sub, inp->data_size,
                (total_sub == inp->data_size) ? "MATCH" : "MISMATCH");
    fprintf(stderr, "\n");
    fprintf(stderr, "[CDM]   loaded_keys(%zu):", state->loaded_key_ids.size());
    for (size_t i = 0; i < state->loaded_key_ids.size(); ++i) {
        fprintf(stderr, " [status=%u id=", (i < state->loaded_key_statuses.size())
                ? state->loaded_key_statuses[i] : 99u);
        for (uint8_t b : state->loaded_key_ids[i]) fprintf(stderr, "%02x", b);
        fprintf(stderr, "]");
    }
    fprintf(stderr, "\n");
    fflush(stderr);
    return out;
}

/// Free memory allocated by cdm_decrypt.
void cdm_free_output(CdmDecryptOutput* out) {
    if (out && out->data) { free(out->data); out->data = nullptr; out->data_size = 0; }
}

/// Close session and unload.
void cdm_destroy(CdmState* state) {
    if (!state) return;
    if (state->cdm)        state->cdm->Destroy();
    delete state->host;
    if (state->deinit)     state->deinit();
    if (state->lib_handle) dlclose(state->lib_handle);
    delete state;
}

} // extern "C"
