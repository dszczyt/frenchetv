/// Safe Rust wrapper around the C CDM shim (cdm_shim.cpp).
///
/// # Thread safety
/// `CdmHandle` is `Send` (guarded externally by `Arc<Mutex<CdmHandle>>`).
/// The underlying CDM is single-threaded; callers must not call methods
/// from multiple threads simultaneously.
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::{Arc, Mutex};
use anyhow::{bail, Context, Result};

// ─── FFI types (must match cdm_shim.cpp exactly) ─────────────────────────────

/// Matches `cdm::SubsampleEntry` in cdm_shim.cpp.
#[repr(C)]
pub struct SubsampleEntry {
    pub clear_bytes: u32,
    pub cipher_bytes: u32,
}

/// Matches `CdmDecryptInput` in cdm_shim.cpp.
#[repr(C)]
pub struct RawDecryptInput {
    pub data: *const u8,
    pub data_size: u32,
    pub encryption_scheme: u32, // 1 = CENC (AES-128-CTR), 2 = CBCS (AES-128-CBC)
    pub key_id: *const u8,
    pub key_id_size: u32,
    pub iv: *const u8,
    pub iv_size: u32,
    pub subsamples: *const SubsampleEntry,
    pub num_subsamples: u32,
    pub timestamp: i64,
}

/// Matches `CdmDecryptOutput` in cdm_shim.cpp.
#[repr(C)]
pub struct RawDecryptOutput {
    pub data: *mut u8,
    pub data_size: u32,
    pub status: c_int, // 0 = kSuccess
}

/// Matches `CdmCallbacks` in cdm_shim.cpp.
#[repr(C)]
struct RawCallbacks {
    on_initialized: Option<unsafe extern "C" fn(*mut c_void, bool)>,
    on_license_request: Option<unsafe extern "C" fn(*mut c_void, *const c_char, u32, *const u8, u32)>,
    on_keys_change: Option<unsafe extern "C" fn(*mut c_void, *const c_char, u32, bool)>,
    on_promise_ok: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_char, u32)>,
    on_promise_err: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_char, u32)>,
    ctx: *mut c_void,
}

unsafe impl Send for RawCallbacks {}
unsafe impl Sync for RawCallbacks {}

/// Opaque CDM state pointer from cdm_shim.cpp.
enum CdmStateOpaque {}

extern "C" {
    fn cdm_create(lib_path: *const c_char, callbacks: *const RawCallbacks) -> *mut CdmStateOpaque;
    fn cdm_initialize(state: *mut CdmStateOpaque);
    fn cdm_create_session(state: *mut CdmStateOpaque, promise_id: u32, pssh: *const u8, pssh_len: u32);
    fn cdm_update_session(
        state: *mut CdmStateOpaque,
        promise_id: u32,
        session_id: *const c_char,
        session_id_len: u32,
        response: *const u8,
        response_len: u32,
    );
    fn cdm_decrypt(state: *mut CdmStateOpaque, inp: *const RawDecryptInput) -> RawDecryptOutput;
    fn cdm_free_output(out: *mut RawDecryptOutput);
    fn cdm_destroy(state: *mut CdmStateOpaque);
}

// ─── Shared callback state ────────────────────────────────────────────────────

#[derive(Default)]
struct CallbackState {
    init_ok: bool,
    session_id: Option<String>,
    license_challenge: Option<Vec<u8>>,
    keys_ready: bool,
    promise_err: Option<String>,
}

// ─── C callback implementations ──────────────────────────────────────────────

unsafe extern "C" fn cb_initialized(ctx: *mut c_void, success: bool) {
    let state = &*(ctx as *const Mutex<CallbackState>);
    if let Ok(mut s) = state.lock() {
        s.init_ok = success;
    }
}

unsafe extern "C" fn cb_license_request(
    ctx: *mut c_void,
    session_id: *const c_char,
    session_id_len: u32,
    request: *const u8,
    request_len: u32,
) {
    let state = &*(ctx as *const Mutex<CallbackState>);
    if let Ok(mut s) = state.lock() {
        let sid_bytes = std::slice::from_raw_parts(session_id as *const u8, session_id_len as usize);
        s.session_id = String::from_utf8(sid_bytes.to_vec()).ok();
        s.license_challenge = Some(std::slice::from_raw_parts(request, request_len as usize).to_vec());
    }
}

unsafe extern "C" fn cb_keys_change(
    ctx: *mut c_void,
    _session_id: *const c_char,
    _session_id_len: u32,
    has_usable_key: bool,
) {
    let state = &*(ctx as *const Mutex<CallbackState>);
    if let Ok(mut s) = state.lock() {
        s.keys_ready = has_usable_key;
    }
}

unsafe extern "C" fn cb_promise_ok(
    ctx: *mut c_void,
    _promise_id: u32,
    session_id: *const c_char,
    session_id_len: u32,
) {
    let state = &*(ctx as *const Mutex<CallbackState>);
    if let Ok(mut s) = state.lock() {
        if !session_id.is_null() && session_id_len > 0 {
            let sid_bytes = std::slice::from_raw_parts(session_id as *const u8, session_id_len as usize);
            s.session_id = String::from_utf8(sid_bytes.to_vec()).ok();
        }
    }
}

unsafe extern "C" fn cb_promise_err(
    ctx: *mut c_void,
    _promise_id: u32,
    msg: *const c_char,
    msg_len: u32,
) {
    let state = &*(ctx as *const Mutex<CallbackState>);
    if let Ok(mut s) = state.lock() {
        let msg_str = if !msg.is_null() && msg_len > 0 {
            let msg_bytes = std::slice::from_raw_parts(msg as *const u8, msg_len as usize);
            String::from_utf8_lossy(msg_bytes).into_owned()
        } else {
            String::from("(no message)")
        };
        s.promise_err = Some(msg_str);
    }
}

// ─── CdmHandle ────────────────────────────────────────────────────────────────

/// Safe wrapper around the C CDM instance.
pub struct CdmHandle {
    raw: *mut CdmStateOpaque,
    /// Shared callback state — lives on the heap so C callbacks can reach it.
    shared: Arc<Mutex<CallbackState>>,
    /// The `Arc` must remain valid for the CDM's lifetime because the raw
    /// `ctx` pointer passed to cdm_create points into it.
    _ctx_arc: Arc<Mutex<CallbackState>>,
}

// The CDM is not thread-safe but we protect it with Arc<Mutex<CdmHandle>>
// in the proxy layer.
unsafe impl Send for CdmHandle {}

impl CdmHandle {
    /// Load `lib_path` (path to `libwidevinecdm.so`), initialise the CDM module,
    /// and create a CDM_10 instance. Returns `Err` if the library is not found
    /// or the CDM interface version is not supported.
    pub fn open(lib_path: &str) -> Result<Self> {
        // The shared state is heap-allocated so its address is stable.
        // We pass `Arc::as_ptr` as ctx; the Arc keeps it alive.
        let shared = Arc::new(Mutex::new(CallbackState::default()));
        // The ctx pointer is the *Mutex<CallbackState>* itself (Arc's inner ptr).
        let ctx_ptr = Arc::as_ptr(&shared) as *mut c_void;

        let callbacks = RawCallbacks {
            on_initialized:    Some(cb_initialized),
            on_license_request: Some(cb_license_request),
            on_keys_change:    Some(cb_keys_change),
            on_promise_ok:     Some(cb_promise_ok),
            on_promise_err:    Some(cb_promise_err),
            ctx: ctx_ptr,
        };

        let lib_c = CString::new(lib_path).context("lib_path contains NUL")?;
        let raw = unsafe { cdm_create(lib_c.as_ptr(), &callbacks) };
        if raw.is_null() {
            bail!("cdm_create failed for '{}'", lib_path);
        }

        Ok(Self { raw, shared: Arc::clone(&shared), _ctx_arc: shared })
    }

    /// Call `CDM::Initialize`.  Must be called once after `open`.
    /// Returns `Err` if the CDM reports failure.
    pub fn initialize(&mut self) -> Result<()> {
        {
            let mut s = self.shared.lock().unwrap();
            s.init_ok = false;
        }
        unsafe { cdm_initialize(self.raw); }
        let s = self.shared.lock().unwrap();
        if !s.init_ok {
            bail!("CDM Initialize returned failure");
        }
        Ok(())
    }

    /// Create a temporary Widevine session with `pssh` init data.
    /// Returns `(session_id, license_challenge_bytes)`.
    ///
    /// The CDM calls `on_license_request` synchronously within this call.
    pub fn create_session(&mut self, pssh: &[u8]) -> Result<(String, Vec<u8>)> {
        {
            let mut s = self.shared.lock().unwrap();
            s.session_id = None;
            s.license_challenge = None;
            s.promise_err = None;
        }
        unsafe { cdm_create_session(self.raw, 1, pssh.as_ptr(), pssh.len() as u32); }
        let s = self.shared.lock().unwrap();
        if let Some(ref err) = s.promise_err {
            bail!("CDM create_session rejected: {}", err);
        }
        let session_id = s.session_id.clone()
            .context("CDM: no session_id after CreateSessionAndGenerateRequest")?;
        let challenge = s.license_challenge.clone()
            .context("CDM: no license challenge after CreateSessionAndGenerateRequest")?;
        Ok((session_id, challenge))
    }

    /// Feed the license server's response into the CDM.
    /// After success `on_keys_change(has_usable_key=true)` has been called.
    pub fn update_session(&mut self, session_id: &str, response: &[u8]) -> Result<()> {
        {
            let mut s = self.shared.lock().unwrap();
            s.keys_ready = false;
            s.promise_err = None;
        }
        let sid_c = CString::new(session_id).context("session_id contains NUL")?;
        unsafe {
            cdm_update_session(
                self.raw,
                2,
                sid_c.as_ptr(),
                session_id.len() as u32,
                response.as_ptr(),
                response.len() as u32,
            );
        }
        let s = self.shared.lock().unwrap();
        if let Some(ref err) = s.promise_err {
            bail!("CDM update_session rejected: {}", err);
        }
        if !s.keys_ready {
            bail!("CDM: no usable keys after UpdateSession");
        }
        Ok(())
    }

    /// Decrypt one encrypted sample.
    ///
    /// * `data` — encrypted sample bytes
    /// * `key_id` — 16-byte key ID
    /// * `iv` — 8 or 16 byte IV
    /// * `subsamples` — `(clear_bytes, cipher_bytes)` pairs; empty = whole sample encrypted
    /// * `timestamp` — decode timestamp for the sample
    /// * `encryption_scheme` — 1 = CENC (AES-128-CTR), 2 = CBCS (AES-128-CBC)
    pub fn decrypt(
        &self,
        data: &[u8],
        key_id: &[u8],
        iv: &[u8],
        subsamples: &[(u32, u32)],
        timestamp: i64,
        encryption_scheme: u32,
    ) -> Result<Vec<u8>> {
        let raw_subsamples: Vec<SubsampleEntry> = subsamples
            .iter()
            .map(|&(c, e)| SubsampleEntry { clear_bytes: c, cipher_bytes: e })
            .collect();

        // CENC (AES-CTR): 8-byte IVs go in the upper 8 bytes of the 16-byte counter block;
        //   lower 8 bytes are zero (CENC spec §9.4).
        // CBCS (AES-CBC pattern): IV is always 16 bytes, used as-is.
        let iv_padded: Vec<u8> = if encryption_scheme != 2 && iv.len() == 8 {
            let mut p = vec![0u8; 16];
            p[..8].copy_from_slice(iv);
            p
        } else {
            iv.to_vec()
        };

        let inp = RawDecryptInput {
            data: data.as_ptr(),
            data_size: data.len() as u32,
            encryption_scheme,
            key_id: key_id.as_ptr(),
            key_id_size: key_id.len() as u32,
            iv: iv_padded.as_ptr(),
            iv_size: iv_padded.len() as u32,
            subsamples: if raw_subsamples.is_empty() { std::ptr::null() } else { raw_subsamples.as_ptr() },
            num_subsamples: raw_subsamples.len() as u32,
            timestamp,
        };

        let mut out = unsafe { cdm_decrypt(self.raw, &inp) };
        if out.status != 0 {
            bail!("CDM Decrypt failed with status {}", out.status);
        }
        if out.data.is_null() {
            bail!("CDM Decrypt returned null data");
        }
        let decrypted = unsafe {
            std::slice::from_raw_parts(out.data, out.data_size as usize).to_vec()
        };
        unsafe { cdm_free_output(&mut out); }
        Ok(decrypted)
    }
}

impl Drop for CdmHandle {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { cdm_destroy(self.raw); }
            self.raw = std::ptr::null_mut();
        }
    }
}
