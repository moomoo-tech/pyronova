//! Safe abstractions for CPython sub-interpreter management.
//!
//! Provides RAII wrappers over raw `pyo3::ffi` pointers to prevent
//! reference count leaks and ensure proper sub-interpreter cleanup.
//! Also implements a channel-based worker pool for true load balancing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use matchit::Router;
use pyo3::ffi;
use pyo3::prelude::*;

// ---------------------------------------------------------------------------
// Phase 7.2: Global worker state for async C-FFI bridge
// ---------------------------------------------------------------------------

// Per-pool configuration (replaces former global statics).
// These are set on PyreApp before run() and propagated to InterpreterPool.
use std::sync::atomic::AtomicBool;

/// Per-worker state accessible from C-FFI functions (no closure environment).
struct WorkerState {
    rx: crossbeam_channel::Receiver<WorkRequest>,
    response_map:
        Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Result<SubInterpResponse, String>>>>,
    next_req_id: AtomicU64,
}

/// Global registry of worker states, indexed by worker_id.
///
/// Must support RE-INSTALLATION: a test suite or a hot-reload path may
/// call `InterpreterPool::new()` more than once per process lifetime.
/// `OnceLock` would silently fail the second `set()`, leaving the
/// second pool with STALE channels from the first pool → permanent
/// deadlock on recv. Use `RwLock<Vec>` instead: read lock on the hot
/// path (~5 ns uncontended), write lock only at pool init.
static WORKER_STATES: std::sync::RwLock<Vec<Arc<WorkerState>>> =
    std::sync::RwLock::new(Vec::new());

fn get_worker_state(worker_id: usize) -> Option<Arc<WorkerState>> {
    WORKER_STATES.read().ok().and_then(|v| v.get(worker_id).cloned())
}

// ---------------------------------------------------------------------------
// C-FFI bridge functions for async engine
// ---------------------------------------------------------------------------

/// pyre_recv(worker_id) → (req_id, handler_idx, method, path, params, query, body, headers, client_ip) or None
/// RELEASES GIL during blocking recv — lets asyncio loop run freely.
unsafe extern "C" fn pyre_recv_cfunc(
    _self: *mut ffi::PyObject,
    args: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    let mut worker_id: isize = 0;
    if ffi::PyArg_ParseTuple(args, c"n".as_ptr(), &mut worker_id) == 0 {
        return std::ptr::null_mut();
    }

    let state = match get_worker_state(worker_id as usize) {
        Some(s) => s,
        None => {
            ffi::Py_INCREF(ffi::Py_None());
            return ffi::Py_None();
        }
    };

    // Release GIL while blocking on channel recv
    let saved = ffi::PyEval_SaveThread();
    let req_opt = state.rx.recv().ok();
    ffi::PyEval_RestoreThread(saved);

    match req_opt {
        Some(req) => {
            let req_id = state.next_req_id.fetch_add(1, Ordering::Relaxed);

            // Build ALL Python objects BEFORE inserting response_tx into map.
            // If any allocation fails, response_tx drops → sender gets error
            // instead of leaking in response_map and causing 504 timeout.
            let py_params = py_str_dict_from_vec(&req.params);
            let py_headers = py_str_dict(&req.headers);
            if py_params.is_none() || py_headers.is_none() {
                // response_tx not inserted → dropped → oneshot Err on Tokio side
                ffi::Py_INCREF(ffi::Py_None());
                return ffi::Py_None();
            }
            let py_params = py_params.unwrap();
            let py_headers = py_headers.unwrap();

            let tuple = ffi::PyTuple_New(9);
            if tuple.is_null() {
                ffi::PyErr_Clear();
                ffi::Py_INCREF(ffi::Py_None());
                return ffi::Py_None();
            }

            // Allocate every leaf Python object UP FRONT and NULL-check
            // before any PyTuple_SetItem call. PyTuple_SetItem steals the
            // reference it's given — embedding a NULL leaks nothing but
            // guarantees a hard segfault the next time anything reads
            // that slot (GC, item access, repr, refcount). Building the
            // full set first lets us reject atomically: if ANY allocation
            // fails, DECREF the successful ones and bail.
            let id_obj = ffi::PyLong_FromUnsignedLongLong(req_id);
            let idx_obj = ffi::PyLong_FromUnsignedLongLong(req.handler_idx as u64);
            let method_obj = ffi::PyUnicode_FromStringAndSize(
                req.method.as_ptr() as *const _,
                req.method.len() as isize,
            );
            let path_obj = ffi::PyUnicode_FromStringAndSize(
                req.path.as_ptr() as *const _,
                req.path.len() as isize,
            );
            let query_obj = ffi::PyUnicode_FromStringAndSize(
                req.query.as_ptr() as *const _,
                req.query.len() as isize,
            );
            let body_obj = ffi::PyBytes_FromStringAndSize(
                req.body.as_ptr() as *const _,
                req.body.len() as isize,
            );
            let ip_obj = ffi::PyUnicode_FromStringAndSize(
                req.client_ip.as_ptr() as *const _,
                req.client_ip.len() as isize,
            );

            let raw_items = [id_obj, idx_obj, method_obj, path_obj, query_obj, body_obj, ip_obj];
            if raw_items.iter().any(|p| p.is_null()) {
                for p in &raw_items {
                    if !p.is_null() {
                        ffi::Py_DECREF(*p);
                    }
                }
                // py_params / py_headers still owned by PyObjRef — dropped here.
                ffi::Py_DECREF(tuple);
                ffi::PyErr_Clear();
                ffi::Py_INCREF(ffi::Py_None());
                return ffi::Py_None();
            }

            // All Python objects built successfully — NOW insert response_tx.
            // Any earlier bail keeps the sender alive; caller's oneshot will
            // close, returning a 503 instead of an orphaned response_map entry.
            state
                .response_map
                .lock()
                .unwrap()
                .insert(req_id, req.response_tx);

            ffi::PyTuple_SetItem(tuple, 0, id_obj);
            ffi::PyTuple_SetItem(tuple, 1, idx_obj);
            ffi::PyTuple_SetItem(tuple, 2, method_obj);
            ffi::PyTuple_SetItem(tuple, 3, path_obj);
            // params / headers as PyDict; PyObjRef.into_raw() transfers ownership.
            ffi::PyTuple_SetItem(tuple, 4, py_params.into_raw());
            ffi::PyTuple_SetItem(tuple, 5, query_obj);
            ffi::PyTuple_SetItem(tuple, 6, body_obj);
            ffi::PyTuple_SetItem(tuple, 7, py_headers.into_raw());
            ffi::PyTuple_SetItem(tuple, 8, ip_obj);
            tuple
        }
        None => {
            ffi::Py_INCREF(ffi::Py_None());
            ffi::Py_None()
        }
    }
}

/// pyre_send(worker_id, req_id, status, content_type, body_bytes)
/// Wakes up Tokio via oneshot channel.
unsafe extern "C" fn pyre_send_cfunc(
    _self: *mut ffi::PyObject,
    args: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    let mut worker_id: isize = 0;
    let mut req_id: u64 = 0;
    let mut status: u16 = 0;
    let mut ctype_str: *const std::os::raw::c_char = std::ptr::null();
    let mut body_ptr: *const std::os::raw::c_char = std::ptr::null();
    let mut body_len: isize = 0;

    // n=isize, K=u64, H=u16, z=str|None, y#=bytes+len
    if ffi::PyArg_ParseTuple(
        args,
        c"nKHzy#".as_ptr(),
        &mut worker_id,
        &mut req_id,
        &mut status,
        &mut ctype_str,
        &mut body_ptr,
        &mut body_len,
    ) == 0
    {
        ffi::PyErr_Print();
        return std::ptr::null_mut();
    }

    let ctype = if !ctype_str.is_null() {
        Some(
            std::ffi::CStr::from_ptr(ctype_str)
                .to_string_lossy()
                .into_owned(),
        )
    } else {
        None
    };

    let body: Vec<u8> = if !body_ptr.is_null() && body_len > 0 {
        let slice = std::slice::from_raw_parts(body_ptr as *const u8, body_len as usize);
        slice.to_vec()
    } else {
        Vec::new()
    };

    if let Some(state) = get_worker_state(worker_id as usize) {
        let mut map = state.response_map.lock().unwrap();
        if let Some(tx) = map.remove(&req_id) {
            // Check if the receiver is still alive (client may have timed out).
            // If closed, skip the send — the response would be discarded anyway.
            if tx.is_closed() {
                tracing::debug!(
                    target: "pyre::server",
                    req_id,
                    worker_id,
                    "response_map: receiver gone (client timed out), dropping result"
                );
            } else {
                let resp = SubInterpResponse {
                    body,
                    status,
                    content_type: ctype,
                    headers: HashMap::new(),
                    is_json: false,
                };
                let _ = tx.send(Ok(resp));
            }
        } else {
            tracing::debug!(
                target: "pyre::server",
                req_id,
                worker_id,
                "response_map miss — client already timed out (504)"
            );
        }

        // Periodic orphan sweep: when the map grows large, purge entries whose
        // receivers have been dropped (Rust side timed out). Prevents unbounded
        // memory growth from handlers that crash after pyre_recv but before pyre_send.
        if map.len() > 64 {
            map.retain(|_id, tx| !tx.is_closed());
        }
    }

    ffi::Py_INCREF(ffi::Py_None());
    ffi::Py_None()
}

// ---------------------------------------------------------------------------
// C-FFI bridge: emit_python_log for sub-interpreter logging
// ---------------------------------------------------------------------------

/// _pyre_emit_log(level, name, message, pathname, lineno, worker_id)
/// Routes Python logging.Handler.emit() calls through Rust tracing.
/// Minimal GIL hold time — extract strings, dispatch to tracing, return.
unsafe extern "C" fn pyre_emit_log_cfunc(
    _self: *mut ffi::PyObject,
    args: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    let mut level_ptr: *const std::os::raw::c_char = std::ptr::null();
    let mut name_ptr: *const std::os::raw::c_char = std::ptr::null();
    let mut msg_ptr: *const std::os::raw::c_char = std::ptr::null();
    let mut path_ptr: *const std::os::raw::c_char = std::ptr::null();
    let mut lineno: i32 = 0;
    let mut worker_id: isize = 0;

    // Parse: (str, str, str, str, int, int)
    if ffi::PyArg_ParseTuple(
        args,
        c"zzzzin".as_ptr(),
        &mut level_ptr,
        &mut name_ptr,
        &mut msg_ptr,
        &mut path_ptr,
        &mut lineno,
        &mut worker_id,
    ) == 0
    {
        // Return None on parse error (don't crash the handler)
        ffi::PyErr_Clear();
        ffi::Py_INCREF(ffi::Py_None());
        return ffi::Py_None();
    }

    let level = if !level_ptr.is_null() {
        std::ffi::CStr::from_ptr(level_ptr)
            .to_str()
            .unwrap_or("INFO")
    } else {
        "INFO"
    };
    let name = if !name_ptr.is_null() {
        std::ffi::CStr::from_ptr(name_ptr)
            .to_str()
            .unwrap_or("unknown")
    } else {
        "unknown"
    };
    let message = if !msg_ptr.is_null() {
        std::ffi::CStr::from_ptr(msg_ptr).to_str().unwrap_or("")
    } else {
        ""
    };
    let pathname = if !path_ptr.is_null() {
        std::ffi::CStr::from_ptr(path_ptr).to_str().unwrap_or("")
    } else {
        ""
    };

    let wid = worker_id as usize;

    match level {
        "DEBUG" => {
            tracing::debug!(
                target: "pyre::app",
                worker = wid,
                logger = %name,
                file = %pathname,
                line = lineno,
                "{}", message
            );
        }
        "INFO" => {
            tracing::info!(
                target: "pyre::app",
                worker = wid,
                logger = %name,
                file = %pathname,
                line = lineno,
                "{}", message
            );
        }
        "WARNING" => {
            tracing::warn!(
                target: "pyre::app",
                worker = wid,
                logger = %name,
                file = %pathname,
                line = lineno,
                "{}", message
            );
        }
        "ERROR" | "CRITICAL" => {
            tracing::error!(
                target: "pyre::app",
                worker = wid,
                logger = %name,
                file = %pathname,
                line = lineno,
                "{}", message
            );
        }
        _ => {
            tracing::trace!(
                target: "pyre::app",
                worker = wid,
                logger = %name,
                file = %pathname,
                line = lineno,
                "{}", message
            );
        }
    }

    ffi::Py_INCREF(ffi::Py_None());
    ffi::Py_None()
}

// ---------------------------------------------------------------------------
// PyObjRef — RAII wrapper for *mut ffi::PyObject
// ---------------------------------------------------------------------------

/// Owned reference to a Python object. Automatically calls `Py_DECREF` on drop.
///
/// # Safety
///
/// Must only be created and dropped while the owning interpreter's GIL is held.
pub(crate) struct PyObjRef {
    ptr: *mut ffi::PyObject,
}

impl PyObjRef {
    /// Wraps a new (owned) reference. Returns `None` if `ptr` is null.
    ///
    /// Caller must ensure `ptr` is a valid new reference (refcount already
    /// incremented by the creating API, e.g. `PyUnicode_FromStringAndSize`).
    pub unsafe fn from_owned(ptr: *mut ffi::PyObject) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            Some(Self { ptr })
        }
    }

    /// Wraps a borrowed reference by incrementing its refcount.
    /// Returns `None` if `ptr` is null.
    pub unsafe fn from_borrowed(ptr: *mut ffi::PyObject) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            ffi::Py_INCREF(ptr);
            Some(Self { ptr })
        }
    }

    /// Returns the raw pointer without consuming the wrapper.
    pub fn as_ptr(&self) -> *mut ffi::PyObject {
        self.ptr
    }

    /// Consumes self and returns the raw pointer **without** decrementing.
    /// Use when transferring ownership (e.g. `PyTuple_SetItem` steals refs).
    pub fn into_raw(self) -> *mut ffi::PyObject {
        let ptr = self.ptr;
        std::mem::forget(self);
        ptr
    }
}

impl Drop for PyObjRef {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                // SAFETY: Py_DECREF requires this thread to have a current
                // tstate (the sub-interp-aware way to say "holds the GIL").
                //
                // DO NOT use `PyGILState_Check()` here: it only returns 1 on
                // the MAIN interpreter thread state; in a sub-interpreter it
                // returns 0 even when the sub-interp's GIL is held. Pairing
                // Py_DECREF with PyGILState_Check silently leaked EVERY
                // PyObject in subinterp mode (~0.75 KB/request at 400k rps —
                // a ~1 GB / minute leak at idle load).
                //
                // `PyThreadState_GetUnchecked()` returns the current tstate
                // if one is attached, NULL otherwise. Attached tstate =>
                // GIL held for its interpreter => DECREF is safe.
                if ffi::PyThreadState_GetUnchecked().is_null() {
                    let t = ffi::Py_TYPE(self.ptr);
                    let type_name = if !t.is_null() {
                        let p = (*t).tp_name;
                        if !p.is_null() {
                            std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
                        } else { "?".into() }
                    } else { "?".into() };
                    let thread_name = std::thread::current()
                        .name()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("{:?}", std::thread::current().id()));
                    let bt = std::backtrace::Backtrace::capture();
                    tracing::error!(
                        target: "pyre::server",
                        ptr = ?self.ptr,
                        type_name = %type_name,
                        thread = %thread_name,
                        backtrace = %bt,
                        "PyObjRef dropped with no attached tstate — leaking pointer to avoid segfault"
                    );
                    return; // Leak is better than crash
                }
                #[cfg(feature = "leak_detect")]
                crate::leak_detect::record_drop(self.ptr);
                ffi::Py_DECREF(self.ptr);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: create Python string from Rust &str
// ---------------------------------------------------------------------------

/// Create a new Python unicode string. Returns an owned `PyObjRef`.
pub(crate) unsafe fn py_str(s: &str) -> Option<PyObjRef> {
    PyObjRef::from_owned(ffi::PyUnicode_FromStringAndSize(
        s.as_ptr() as *const _,
        s.len() as isize,
    ))
}

/// Create a new Python bytes object. Returns an owned `PyObjRef`.
pub(crate) unsafe fn py_bytes(data: &[u8]) -> Option<PyObjRef> {
    PyObjRef::from_owned(ffi::PyBytes_FromStringAndSize(
        data.as_ptr() as *const _,
        data.len() as isize,
    ))
}

/// Create a new Python dict from a HashMap<String, String>. Returns owned `PyObjRef`.
///
/// On any failure (str alloc OOM or PyDict_SetItem failure), clears the
/// pending Python exception before returning None. Callers must not
/// pass a non-NULL PyObject back to Python with a set exception state
/// (CPython raises SystemError in that case).
pub(crate) unsafe fn py_str_dict(map: &HashMap<String, String>) -> Option<PyObjRef> {
    let dict = PyObjRef::from_owned(ffi::PyDict_New())?;
    for (k, v) in map {
        let pk = match py_str(k) {
            Some(p) => p,
            None => {
                ffi::PyErr_Clear();
                return None;
            }
        };
        let pv = match py_str(v) {
            Some(p) => p,
            None => {
                ffi::PyErr_Clear();
                return None;
            }
        };
        if ffi::PyDict_SetItem(dict.as_ptr(), pk.as_ptr(), pv.as_ptr()) < 0 {
            ffi::PyErr_Clear();
            return None;
        }
    }
    Some(dict)
}

/// Same as `py_str_dict` but from a Vec of key-value pairs (for path params).
///
/// Same exception-clearing discipline as `py_str_dict` — see doc there.
pub(crate) unsafe fn py_str_dict_from_vec(pairs: &[(String, String)]) -> Option<PyObjRef> {
    let dict = PyObjRef::from_owned(ffi::PyDict_New())?;
    for (k, v) in pairs {
        let pk = match py_str(k) {
            Some(p) => p,
            None => {
                ffi::PyErr_Clear();
                return None;
            }
        };
        let pv = match py_str(v) {
            Some(p) => p,
            None => {
                ffi::PyErr_Clear();
                return None;
            }
        };
        if ffi::PyDict_SetItem(dict.as_ptr(), pk.as_ptr(), pv.as_ptr()) < 0 {
            ffi::PyErr_Clear();
            return None;
        }
    }
    Some(dict)
}

/// Extract a Rust String from a Python str object (raw FFI).
pub(crate) unsafe fn pyobj_to_string(obj: *mut ffi::PyObject) -> Result<String, String> {
    let mut size: isize = 0;
    let ptr = ffi::PyUnicode_AsUTF8AndSize(obj, &mut size);
    if ptr.is_null() {
        ffi::PyErr_Clear(); // Must clear exception before any further C-API calls
        return Err("failed to extract string".to_string());
    }
    let bytes = std::slice::from_raw_parts(ptr as *const u8, size as usize);
    String::from_utf8(bytes.to_vec()).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Sub-interpreter response
// ---------------------------------------------------------------------------

/// Result from a sub-interpreter handler call.
pub(crate) struct SubInterpResponse {
    pub body: Vec<u8>,
    pub status: u16,
    pub content_type: Option<String>,
    pub headers: HashMap<String, String>,
    pub is_json: bool,
}

// ---------------------------------------------------------------------------
// Work item for channel-based dispatch
// ---------------------------------------------------------------------------

pub(crate) struct WorkRequest {
    pub handler_idx: usize,
    pub method: String,
    pub path: String,
    pub params: Vec<(String, String)>,
    pub query: String,
    pub body: bytes::Bytes,
    pub headers: HashMap<String, String>,
    pub client_ip: String,
    pub response_tx: tokio::sync::oneshot::Sender<Result<SubInterpResponse, String>>,
}

// Diagnostic: count WorkRequest creates vs worker-completes. We cannot
// use a Drop impl here because `response_tx` gets moved out inside the
// worker loop (`req.response_tx.send(...)`) — a Drop on WorkRequest
// would forbid destructuring. Instead we bump the counter at each
// site where a request finishes its turn through the pipeline.
static WR_CREATED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static WR_COMPLETED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl WorkRequest {
    pub fn inc_created() {
        WR_CREATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn inc_completed() {
        WR_COMPLETED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn created_count() -> u64 {
        WR_CREATED.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn dropped_count() -> u64 {
        WR_COMPLETED.load(std::sync::atomic::Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Safe sub-interpreter
// ---------------------------------------------------------------------------

struct SubInterpreterWorker {
    /// Thread state (saved after releasing GIL)
    tstate: *mut ffi::PyThreadState,
    /// Handler function pointers keyed by name
    handlers: HashMap<String, *mut ffi::PyObject>,
    /// Globals dict of this sub-interpreter
    globals: *mut ffi::PyObject,
    /// Cached: json.dumps function pointer (avoids per-request import)
    json_dumps_func: *mut ffi::PyObject,
    /// Cached: `_PyreRequest` **type object** (raw C-API heap type,
    /// defined in `pyre_request_type.rs`). Built per sub-interp via
    /// `PyType_FromSpec` so its custom `tp_dealloc` can synchronously
    /// DECREF all slot fields — workaround for PEP 684's broken
    /// per-instance dealloc path on `__slots__` Python classes.
    sky_request_cls: *mut ffi::PyObject,
    /// Cached: _PyreResponse class pointer
    sky_response_cls: *mut ffi::PyObject,
    /// Cached: persistent asyncio event loop for this sub-interpreter
    _asyncio_loop: *mut ffi::PyObject,
    /// Cached: loop.run_until_complete method
    loop_run_func: *mut ffi::PyObject,
}

unsafe impl Send for SubInterpreterWorker {}

impl SubInterpreterWorker {
    /// Create a new sub-interpreter, execute the filtered script, extract handlers.
    ///
    /// # Safety
    /// Must be called while the main interpreter's thread state is current.
    /// Switches to the new sub-interpreter and back to main on completion.
    unsafe fn new(script: &str, script_path: &str, func_names: &[String]) -> Result<Self, String> {
        let main_tstate = ffi::PyThreadState_Get();

        let mut new_tstate: *mut ffi::PyThreadState = std::ptr::null_mut();
        let config = ffi::PyInterpreterConfig {
            use_main_obmalloc: 0,
            allow_fork: 0,
            allow_exec: 0,
            allow_threads: 1,
            allow_daemon_threads: 0,
            check_multi_interp_extensions: 1, // Strict: only extensions declaring multi-interp support
            gil: ffi::PyInterpreterConfig_OWN_GIL,
        };

        let status = ffi::Py_NewInterpreterFromConfig(&mut new_tstate, &config);
        if ffi::PyStatus_IsError(status) != 0 || new_tstate.is_null() {
            ffi::PyThreadState_Swap(main_tstate);
            return Err("Py_NewInterpreterFromConfig failed".to_string());
        }

        // Past this point we own a live sub-interpreter. Any early error
        // must Py_EndInterpreter it before returning, or the sub-interp
        // (and the thread resources it pins) leak permanently. Delegate
        // init to a helper so `?` can short-circuit safely — we catch its
        // Err here and perform cleanup regardless of which step failed.
        match Self::init_in_sub_interp(script, script_path, func_names) {
            Ok(worker) => {
                ffi::PyThreadState_Swap(main_tstate);
                Ok(worker)
            }
            Err(e) => {
                ffi::Py_EndInterpreter(ffi::PyThreadState_Get());
                ffi::PyThreadState_Swap(main_tstate);
                Err(e)
            }
        }
    }

    /// Run every init step that executes INSIDE the freshly-created
    /// sub-interpreter. Returns a worker whose `tstate` is already saved
    /// via PyEval_SaveThread (GIL released). Caller is responsible for
    /// swapping back to the main tstate, and for Py_EndInterpreter on error.
    ///
    /// # Safety
    /// Must be called with a sub-interpreter's thread state current.
    unsafe fn init_in_sub_interp(
        script: &str,
        script_path: &str,
        func_names: &[String],
    ) -> Result<Self, String> {
        // Run the bootstrap (from external .py file) + user script.
        let bootstrap_src = include_str!("../python/pyreframework/_bootstrap.py");
        let bootstrap = format!("{bootstrap_src}\n# Execute full user script\n{script}");

        let globals =
            PyObjRef::from_owned(ffi::PyDict_New()).ok_or("failed to create globals dict")?;
        let builtins = ffi::PyEval_GetBuiltins(); // borrowed ref
        ffi::PyDict_SetItemString(globals.as_ptr(), c"__builtins__".as_ptr(), builtins);

        // Register _pyre_emit_log C-FFI function for Python logging bridge
        #[allow(clippy::missing_transmute_annotations)]
        let emit_log_def = Box::into_raw(Box::new(ffi::PyMethodDef {
            ml_name: c"_pyre_emit_log".as_ptr(),
            ml_meth: ffi::PyMethodDefPointer {
                PyCFunctionWithKeywords: std::mem::transmute(pyre_emit_log_cfunc as *const ()),
            },
            ml_flags: ffi::METH_VARARGS,
            ml_doc: std::ptr::null(),
        }));
        let emit_log_func =
            ffi::PyCFunction_NewEx(emit_log_def, std::ptr::null_mut(), std::ptr::null_mut());
        if !emit_log_func.is_null() {
            ffi::PyDict_SetItemString(globals.as_ptr(), c"_pyre_emit_log".as_ptr(), emit_log_func);
            ffi::Py_DECREF(emit_log_func);
        }

        // Set __file__ so user scripts can use it for path resolution
        if let Some(py_file) = py_str(script_path) {
            ffi::PyDict_SetItemString(globals.as_ptr(), c"__file__".as_ptr(), py_file.as_ptr());
        }

        let code_cstr = std::ffi::CString::new(bootstrap.as_bytes())
            .map_err(|e| format!("CString error: {e}"))?;
        let _filename_cstr =
            std::ffi::CString::new(script_path).map_err(|e| format!("CString error: {e}"))?;

        let result = PyObjRef::from_owned(ffi::PyRun_String(
            code_cstr.as_ptr(),
            ffi::Py_file_input,
            globals.as_ptr(),
            globals.as_ptr(),
        ));

        if result.is_none() {
            ffi::PyErr_Print();
            // globals dropped here → DECREF. Outer `new()` destroys the
            // sub-interpreter and swaps back to main once we return Err.
            return Err("failed to execute script in sub-interpreter".to_string());
        }
        // result dropped here → DECREF (it's just Py_None for exec)

        // Extract handler functions by name
        let mut handlers = HashMap::new();
        for name in func_names {
            let name_cstr = std::ffi::CString::new(name.as_bytes())
                .map_err(|e| format!("CString error: {e}"))?;
            let func = ffi::PyDict_GetItemString(globals.as_ptr(), name_cstr.as_ptr());
            if !func.is_null() && ffi::PyCallable_Check(func) != 0 {
                ffi::Py_INCREF(func);
                handlers.insert(name.clone(), func);
            }
        }

        // Build the raw C-API `_PyreRequest` type for THIS sub-interp
        // (custom tp_dealloc that synchronously DECREFs every slot —
        // workaround for PEP 684's broken heap-type finalizer). One
        // type per sub-interp: PyTypeObject state is per-interp.
        //
        // We then install helper methods (`.text()`, `.json()`, `.body`,
        // `.query_params`) DIRECTLY on the heap type — NOT via a
        // Python subclass. A subclass triggers CPython's subtype_dealloc
        // fallback and bypasses our custom tp_dealloc, restoring the
        // full-instance leak we're trying to fix.
        let rust_ty = crate::pyre_request_type::register_type()?;
        let req_cls_name = std::ffi::CString::new("_PyreRequest").unwrap();
        if ffi::PyDict_SetItemString(globals.as_ptr(), req_cls_name.as_ptr(), rust_ty) != 0 {
            ffi::PyErr_Print();
            return Err("failed to inject _PyreRequest into sub-interp globals".to_string());
        }

        // Attach helper methods directly on the type (mutable by
        // virtue of Py_TPFLAGS_HEAPTYPE). Users can do
        // `req.text()` / `req.json()` / `req.body` / `req.query_params`.
        let helpers_src = c"\
def _attach_pyre_request_helpers(t):\n    from urllib.parse import parse_qs\n    import json as _json\n    t.body = property(lambda self: self.body_bytes)\n    t.query_params = property(lambda self: {k: v[0] for k, v in parse_qs(self.query).items()})\n    t.text = lambda self: self.body_bytes.decode('utf-8') if isinstance(self.body_bytes, (bytes, bytearray)) else str(self.body_bytes)\n    t.json = lambda self: _json.loads(self.text())\n_attach_pyre_request_helpers(_PyreRequest)\n";
        let helpers_result = ffi::PyRun_String(
            helpers_src.as_ptr(),
            ffi::Py_file_input,
            globals.as_ptr(),
            globals.as_ptr(),
        );
        if helpers_result.is_null() {
            ffi::PyErr_Print();
            return Err("failed to attach _PyreRequest helper methods".to_string());
        }
        ffi::Py_DECREF(helpers_result);

        let sky_request_cls = rust_ty;
        ffi::Py_INCREF(sky_request_cls);

        let resp_cls_name = std::ffi::CString::new("_PyreResponse").unwrap();
        let sky_response_cls = ffi::PyDict_GetItemString(globals.as_ptr(), resp_cls_name.as_ptr());
        if !sky_response_cls.is_null() {
            ffi::Py_INCREF(sky_response_cls);
        }

        // Try orjson first (10-40x faster than stdlib json), fall back to json
        let json_dumps_func = {
            let orjson_mod = ffi::PyImport_ImportModule(c"orjson".as_ptr());
            if !orjson_mod.is_null() {
                let f = ffi::PyObject_GetAttrString(orjson_mod, c"dumps".as_ptr());
                ffi::Py_DECREF(orjson_mod);
                f
            } else {
                ffi::PyErr_Clear();
                let json_mod = ffi::PyImport_ImportModule(c"json".as_ptr());
                if !json_mod.is_null() {
                    let f = ffi::PyObject_GetAttrString(json_mod, c"dumps".as_ptr());
                    ffi::Py_DECREF(json_mod);
                    f
                } else {
                    ffi::PyErr_Clear();
                    std::ptr::null_mut()
                }
            }
        };

        // Create persistent asyncio event loop for this sub-interpreter
        let (asyncio_loop, loop_run_func) = {
            let asyncio_mod = ffi::PyImport_ImportModule(c"asyncio".as_ptr());
            if !asyncio_mod.is_null() {
                let loop_obj = ffi::PyObject_CallMethod(
                    asyncio_mod,
                    c"new_event_loop".as_ptr(),
                    std::ptr::null(),
                );
                let run_func = if !loop_obj.is_null() {
                    // Set as current loop
                    ffi::PyObject_CallMethod(
                        asyncio_mod,
                        c"set_event_loop".as_ptr(),
                        c"O".as_ptr(),
                        loop_obj,
                    );
                    ffi::PyObject_GetAttrString(loop_obj, c"run_until_complete".as_ptr())
                } else {
                    ffi::PyErr_Clear();
                    std::ptr::null_mut()
                };
                ffi::Py_DECREF(asyncio_mod);
                (loop_obj, run_func)
            } else {
                ffi::PyErr_Clear();
                (std::ptr::null_mut(), std::ptr::null_mut())
            }
        };

        // Keep globals alive — transfer ownership to the struct
        let globals_ptr = globals.into_raw();

        // Release this sub-interpreter's GIL. Outer `new()` swaps back to
        // the main interpreter after we return.
        let saved = ffi::PyEval_SaveThread();

        Ok(SubInterpreterWorker {
            tstate: saved,
            handlers,
            globals: globals_ptr,
            json_dumps_func,
            sky_request_cls,
            sky_response_cls,
            _asyncio_loop: asyncio_loop,
            loop_run_func,
        })
    }

    /// Build a fresh `_PyreRequest` instance for this request.
    ///
    /// Returns a NEW owned reference (caller must DECREF). The
    /// instance's `tp_dealloc` synchronously DECREFs all slot fields,
    /// so no `SlotClearer` / instance recycling is needed.
    ///
    /// # Safety
    /// Must be called with this sub-interpreter's GIL held.
    #[allow(clippy::too_many_arguments)]
    unsafe fn build_request(
        &self,
        method: &str,
        path: &str,
        params: &[(String, String)],
        query: &str,
        body: &[u8],
        headers: &HashMap<String, String>,
        client_ip: &str,
    ) -> Result<*mut ffi::PyObject, String> {
        // ── Leak-hunt bisection of slot constructors (leak_detect only) ──
        // PYRE_BISECT_SLOT=<name> replaces that slot's value with Py_None:
        //   method | path | params | query | body | headers | client_ip
        //   all    — every slot becomes None (alloc shell only)
        //   none   — normal (default)
        #[cfg(feature = "leak_detect")]
        let slot_mode = std::env::var("PYRE_BISECT_SLOT").ok();
        #[cfg(not(feature = "leak_detect"))]
        let slot_mode: Option<String> = None;
        let skip = |name: &str| -> bool {
            match slot_mode.as_deref() {
                Some("all") => true,
                Some(s) => s == name,
                None => false,
            }
        };
        let none_ref = || unsafe { PyObjRef::from_borrowed(ffi::Py_None()).unwrap() };

        let py_method = if skip("method") {
            none_ref()
        } else {
            py_str(method).ok_or("failed to create py_method")?
        };
        let py_path = if skip("path") {
            none_ref()
        } else {
            py_str(path).ok_or("failed to create py_path")?
        };
        let py_params = if skip("params") {
            none_ref()
        } else {
            py_str_dict_from_vec(params).ok_or("failed to create py_params")?
        };
        let py_query = if skip("query") {
            none_ref()
        } else {
            py_str(query).ok_or("failed to create py_query")?
        };
        let py_body = if skip("body") {
            none_ref()
        } else {
            py_bytes(body).ok_or("failed to create py_body")?
        };
        let py_headers = if skip("headers") {
            none_ref()
        } else {
            py_str_dict(headers).ok_or("failed to create py_headers")?
        };
        let py_client_ip = if skip("client_ip") {
            none_ref()
        } else {
            py_str(client_ip).ok_or("failed to create py_client_ip")?
        };

        if self.sky_request_cls.is_null() {
            return Err("_PyreRequest type not registered".to_string());
        }

        // Transfer ownership of each new ref into the instance.
        // `alloc_and_init` DECREFs them on failure — so we must NOT
        // rely on PyObjRef::Drop after `into_raw()`.
        crate::pyre_request_type::alloc_and_init(
            self.sky_request_cls,
            py_method.into_raw(),
            py_path.into_raw(),
            py_params.into_raw(),
            py_query.into_raw(),
            py_body.into_raw(),
            py_headers.into_raw(),
            py_client_ip.into_raw(),
        )
    }

    /// Parse a handler return value into SubInterpResponse.
    ///
    /// # Safety
    /// Must be called with this sub-interpreter's GIL held.
    unsafe fn parse_result(&self, result_obj: PyObjRef) -> Result<SubInterpResponse, String> {
        let ptr = result_obj.as_ptr();

        // Check if it's a _PyreResponse or any response-like object
        // (duck typing: has status_code + body attributes).
        //
        // PyObject_IsInstance returns 1 (true), 0 (false), or -1 (error
        // with exception set). Treating -1 as false without clearing
        // the exception is a SystemError latent bomb — the next C-API
        // call short-circuits on the pending exception.
        let resp_cls = self.sky_response_cls;
        let is_response = if resp_cls.is_null() {
            false
        } else {
            match ffi::PyObject_IsInstance(ptr, resp_cls) {
                1 => true,
                -1 => {
                    ffi::PyErr_Clear();
                    // Fall through to duck-type check.
                    let has_status =
                        ffi::PyObject_HasAttrString(ptr, c"status_code".as_ptr()) == 1;
                    let has_body = ffi::PyObject_HasAttrString(ptr, c"body".as_ptr()) == 1;
                    has_status && has_body
                }
                _ => {
                    // 0 (not an instance) — try duck-typing.
                    let has_status =
                        ffi::PyObject_HasAttrString(ptr, c"status_code".as_ptr()) == 1;
                    let has_body = ffi::PyObject_HasAttrString(ptr, c"body".as_ptr()) == 1;
                    has_status && has_body
                }
            }
        };
        if is_response {
            return self.parse_sky_response(result_obj);
        }

        // dict → JSON
        if ffi::PyDict_Check(ptr) != 0 {
            let json_str = self.json_dumps(result_obj)?;
            return Ok(SubInterpResponse {
                body: json_str.into_bytes(),
                status: 200,
                content_type: None,
                headers: HashMap::new(),
                is_json: true,
            });
        }

        // string
        if ffi::PyUnicode_Check(ptr) != 0 {
            let s = pyobj_to_string(ptr)?;
            return Ok(SubInterpResponse {
                body: s.into_bytes(),
                status: 200,
                content_type: None,
                headers: HashMap::new(),
                is_json: false,
            });
        }

        // fallback: str(result)
        let str_obj = PyObjRef::from_owned(ffi::PyObject_Str(ptr)).ok_or_else(|| {
            ffi::PyErr_Clear();
            "str() failed".to_string()
        })?;
        let s = pyobj_to_string(str_obj.as_ptr())?;
        Ok(SubInterpResponse {
            body: s.into_bytes(),
            status: 200,
            content_type: None,
            headers: HashMap::new(),
            is_json: false,
        })
    }

    /// Build a _PyreResponse Python object from a SubInterpResponse.
    unsafe fn build_sky_response(&self, resp: &SubInterpResponse) -> Result<PyObjRef, String> {
        if self.sky_response_cls.is_null() {
            return Err("_PyreResponse class not available".to_string());
        }

        // Convert body to Python object — use bytes for binary, str for text
        let py_body = if resp.is_json || std::str::from_utf8(&resp.body).is_ok() {
            let body_str = unsafe { std::str::from_utf8_unchecked(&resp.body) };
            py_str(body_str).ok_or("failed to create body str")?
        } else {
            // Binary data: use PyBytes to avoid UTF-8 corruption
            PyObjRef::from_owned(ffi::PyBytes_FromStringAndSize(
                resp.body.as_ptr() as *const _,
                resp.body.len() as isize,
            ))
            .ok_or("failed to create body bytes")?
        };
        let py_status = PyObjRef::from_owned(ffi::PyLong_FromLong(resp.status as i64))
            .ok_or("failed to create status")?;
        let py_ct = match &resp.content_type {
            Some(ct) => py_str(ct).ok_or("failed to create content_type")?,
            None => PyObjRef::from_borrowed(ffi::Py_None()).unwrap(),
        };
        let py_headers = py_str_dict(&resp.headers).ok_or("failed to create headers dict")?;

        // _PyreResponse(body, status_code, content_type, headers)
        let args = PyObjRef::from_owned(ffi::PyTuple_New(0)).ok_or("failed to create args")?;
        let kwargs = PyObjRef::from_owned(ffi::PyDict_New()).ok_or("failed to create kwargs")?;

        ffi::PyDict_SetItemString(kwargs.as_ptr(), c"body".as_ptr(), py_body.as_ptr());
        ffi::PyDict_SetItemString(kwargs.as_ptr(), c"status_code".as_ptr(), py_status.as_ptr());
        ffi::PyDict_SetItemString(kwargs.as_ptr(), c"content_type".as_ptr(), py_ct.as_ptr());
        ffi::PyDict_SetItemString(kwargs.as_ptr(), c"headers".as_ptr(), py_headers.as_ptr());

        PyObjRef::from_owned(ffi::PyObject_Call(
            self.sky_response_cls,
            args.as_ptr(),
            kwargs.as_ptr(),
        ))
        .ok_or_else(|| {
            ffi::PyErr_Print();
            "failed to create _PyreResponse".to_string()
        })
    }

    /// If obj is awaitable (coroutine / Task / Future / custom __await__),
    /// drive it via the persistent event loop. Otherwise return unchanged.
    ///
    /// Broader than the historical `PyCoro_CheckExact` — handlers/hooks
    /// legitimately return `asyncio.create_task(...)` or user-defined
    /// Awaitable objects, and treating those as live responses produced
    /// `"<coroutine object ... at 0x...>"` strings in the HTTP body.
    /// Fast-path PyCoro_CheckExact first (single tag compare), fall
    /// through to `hasattr __await__` for the minority path.
    unsafe fn resolve_coroutine(&self, obj: PyObjRef) -> Result<PyObjRef, String> {
        let is_coro = ffi::PyCoro_CheckExact(obj.as_ptr()) == 1;
        let is_awaitable = is_coro
            || ffi::PyObject_HasAttrString(obj.as_ptr(), c"__await__".as_ptr()) == 1;
        if !is_awaitable {
            return Ok(obj); // Plain value — pass through
        }
        if self.loop_run_func.is_null() {
            return Err("async handler used but asyncio event loop not available".to_string());
        }
        // Call loop.run_until_complete(awaitable)
        let args =
            PyObjRef::from_owned(ffi::PyTuple_New(1)).ok_or("failed to create args tuple")?;
        ffi::PyTuple_SetItem(args.as_ptr(), 0, obj.into_raw());
        let result = PyObjRef::from_owned(ffi::PyObject_Call(
            self.loop_run_func,
            args.as_ptr(),
            std::ptr::null_mut(),
        ));
        match result {
            Some(r) => Ok(r),
            None => {
                ffi::PyErr_Print();
                Err("loop.run_until_complete() failed".to_string())
            }
        }
    }

    /// Serialize a Python dict/list to JSON string via cached dumps (orjson or json).
    unsafe fn json_dumps(&self, obj: PyObjRef) -> Result<String, String> {
        if self.json_dumps_func.is_null() {
            return Err("json.dumps not cached".to_string());
        }

        let args = PyObjRef::from_owned(ffi::PyTuple_New(1)).ok_or("failed to create tuple")?;
        ffi::PyTuple_SetItem(args.as_ptr(), 0, obj.into_raw());

        let result = PyObjRef::from_owned(ffi::PyObject_Call(
            self.json_dumps_func,
            args.as_ptr(),
            std::ptr::null_mut(),
        ))
        .ok_or_else(|| {
            ffi::PyErr_Print();
            "json.dumps failed".to_string()
        })?;

        // orjson.dumps returns bytes, json.dumps returns str
        if ffi::PyBytes_Check(result.as_ptr()) != 0 {
            let ptr = ffi::PyBytes_AsString(result.as_ptr());
            let size = ffi::PyBytes_Size(result.as_ptr());
            if ptr.is_null() {
                return Err("failed to extract bytes".to_string());
            }
            let bytes = std::slice::from_raw_parts(ptr as *const u8, size as usize);
            String::from_utf8(bytes.to_vec()).map_err(|e| e.to_string())
        } else {
            pyobj_to_string(result.as_ptr())
        }
    }

    /// Parse a _PyreResponse Python object.
    unsafe fn parse_sky_response(&self, obj: PyObjRef) -> Result<SubInterpResponse, String> {
        let ptr = obj.as_ptr();

        // status_code
        let status = {
            let attr =
                PyObjRef::from_owned(ffi::PyObject_GetAttrString(ptr, c"status_code".as_ptr()));
            match attr {
                Some(a) => {
                    let code = ffi::PyLong_AsLong(a.as_ptr());
                    if code == -1 && !ffi::PyErr_Occurred().is_null() {
                        ffi::PyErr_Clear();
                        200
                    } else {
                        code as u16
                    }
                }
                None => {
                    ffi::PyErr_Clear();
                    200
                }
            }
        };

        // content_type
        let content_type = {
            let attr =
                PyObjRef::from_owned(ffi::PyObject_GetAttrString(ptr, c"content_type".as_ptr()));
            match attr {
                Some(a) if a.as_ptr() != ffi::Py_None() => pyobj_to_string(a.as_ptr()).ok(),
                _ => {
                    ffi::PyErr_Clear();
                    None
                }
            }
        };

        // headers
        //
        // CRITICAL: PyDict_Next forbids dict mutation during iteration.
        // PyObject_Str may invoke user __str__ which could mutate the
        // dict → undefined behaviour / segfault. We collect borrowed
        // key/value refs first, INCREF them, then release the iteration
        // scope before calling any method that may re-enter Python.
        let mut resp_headers = HashMap::new();
        {
            let attr = PyObjRef::from_owned(ffi::PyObject_GetAttrString(ptr, c"headers".as_ptr()));
            if let Some(a) = &attr {
                if ffi::PyDict_Check(a.as_ptr()) != 0 {
                    // Phase 1: snapshot (no user code runs).
                    let mut snapshot: Vec<(PyObjRef, PyObjRef)> = Vec::new();
                    let mut pos: isize = 0;
                    let mut key: *mut ffi::PyObject = std::ptr::null_mut();
                    let mut val: *mut ffi::PyObject = std::ptr::null_mut();
                    while ffi::PyDict_Next(a.as_ptr(), &mut pos, &mut key, &mut val) != 0 {
                        // PyDict_Next returns borrowed refs — INCREF to own them.
                        if let (Some(k), Some(v)) = (
                            PyObjRef::from_borrowed(key),
                            PyObjRef::from_borrowed(val),
                        ) {
                            snapshot.push((k, v));
                        }
                    }
                    // Phase 2: convert — safe to invoke __str__ now.
                    for (k_obj, v_obj) in snapshot {
                        let str_key = PyObjRef::from_owned(ffi::PyObject_Str(k_obj.as_ptr()));
                        let str_val = PyObjRef::from_owned(ffi::PyObject_Str(v_obj.as_ptr()));
                        if let (Some(sk), Some(sv)) = (str_key, str_val) {
                            if let (Ok(k), Ok(v)) =
                                (pyobj_to_string(sk.as_ptr()), pyobj_to_string(sv.as_ptr()))
                            {
                                resp_headers.insert(k, v);
                            }
                        } else {
                            ffi::PyErr_Clear();
                        }
                    }
                }
            }
            ffi::PyErr_Clear();
        }

        // body (returns Vec<u8>)
        let (body, is_json): (Vec<u8>, bool) = {
            let attr = PyObjRef::from_owned(ffi::PyObject_GetAttrString(ptr, c"body".as_ptr()));
            match attr {
                Some(a) => {
                    if ffi::PyDict_Check(a.as_ptr()) != 0 {
                        match self.json_dumps(a) {
                            Ok(s) => (s.into_bytes(), true),
                            Err(e) => {
                                tracing::error!(
                                    target: "pyre::server",
                                    error = %e,
                                    "JSON serialization failed for response body dict"
                                );
                                let msg =
                                    format!(r#"{{"error":"json serialization failed: {}"}}"#, e);
                                return Ok(SubInterpResponse {
                                    body: msg.into_bytes(),
                                    status: 500,
                                    content_type: Some("application/json".to_string()),
                                    headers: resp_headers,
                                    is_json: true,
                                });
                            }
                        }
                    } else if ffi::PyBytes_Check(a.as_ptr()) != 0 {
                        // Raw bytes — pass through without UTF-8 conversion
                        let size = ffi::PyBytes_Size(a.as_ptr());
                        let ptr = ffi::PyBytes_AsString(a.as_ptr());
                        if !ptr.is_null() && size > 0 {
                            let slice = std::slice::from_raw_parts(ptr as *const u8, size as usize);
                            (slice.to_vec(), false)
                        } else {
                            (Vec::new(), false)
                        }
                    } else if ffi::PyUnicode_Check(a.as_ptr()) != 0 {
                        (
                            pyobj_to_string(a.as_ptr()).unwrap_or_default().into_bytes(),
                            false,
                        )
                    } else {
                        let str_obj = PyObjRef::from_owned(ffi::PyObject_Str(a.as_ptr()));
                        match str_obj {
                            Some(s) => (
                                pyobj_to_string(s.as_ptr()).unwrap_or_default().into_bytes(),
                                false,
                            ),
                            None => {
                                ffi::PyErr_Clear();
                                (Vec::new(), false)
                            }
                        }
                    }
                }
                None => {
                    ffi::PyErr_Clear();
                    (Vec::new(), false)
                }
            }
        };

        Ok(SubInterpResponse {
            body,
            status,
            content_type,
            headers: resp_headers,
            is_json,
        })
    }

    /// Call a handler function and return the response.
    ///
    /// # Safety
    /// Must be called with this sub-interpreter's GIL held.
    #[allow(clippy::too_many_arguments)]
    unsafe fn call_handler(
        &mut self,
        handler_name: &str,
        before_hook_names: &[String],
        after_hook_names: &[String],
        method: &str,
        path: &str,
        params: &[(String, String)],
        query: &str,
        body: &[u8],
        headers: &HashMap<String, String>,
        client_ip: &str,
    ) -> Result<SubInterpResponse, String> {
        let func = *self
            .handlers
            .get(handler_name)
            .ok_or_else(|| format!("handler '{}' not found", handler_name))?;

        // Fresh `_PyreRequest` (new owned ref). The Rust-backed type's
        // `tp_dealloc` synchronously DECREFs all slot fields when this
        // PyObjRef drops at scope end — no SlotClearer / instance
        // recycling needed, no PEP 684 finalizer bug to work around.
        // ── Leak-hunt bisection hook (leak_detect feature only) ──────
        // PYRE_BISECT values:
        //   "skip_all"     — no build_request, no handler call; return a
        //                    fixed SubInterpResponse. Exercises hyper +
        //                    channel only. If this still leaks, the
        //                    leak is NOT in the Python side at all.
        //   "skip_handler" — build_request + dealloc runs, but handler
        //                    is not invoked. Isolates request-object
        //                    construction from handler/response path.
        //   "skip_build"   — handler runs with Py_None as the request
        //                    arg (user code will crash, but we only
        //                    care about memory — use a handler that
        //                    ignores its arg, e.g. `def h(req): return "ok"`).
        // Unset / any other value: normal execution.
        #[cfg(feature = "leak_detect")]
        let bisect_mode = std::env::var("PYRE_BISECT").ok();
        #[cfg(not(feature = "leak_detect"))]
        let bisect_mode: Option<String> = None;

        if bisect_mode.as_deref() == Some("skip_all") {
            return Ok(SubInterpResponse {
                body: b"ok".to_vec(),
                status: 200,
                content_type: None,
                headers: HashMap::new(),
                is_json: false,
            });
        }

        let request_ref: Option<PyObjRef> = if bisect_mode.as_deref() == Some("skip_build") {
            // Hand the handler Py_None instead of a built request.
            Some(PyObjRef::from_borrowed(ffi::Py_None()).unwrap())
        } else {
            Some(
                PyObjRef::from_owned(
                    self.build_request(method, path, params, query, body, headers, client_ip)?,
                )
                .ok_or("build_request returned null")?,
            )
        };
        let request = request_ref.unwrap();
        let request_ptr = request.as_ptr();

        if bisect_mode.as_deref() == Some("skip_handler") {
            // Drop request (triggers tp_dealloc) and return a fixed
            // response — skips hooks, Vectorcall, parse_result.
            drop(request);
            return Ok(SubInterpResponse {
                body: b"ok".to_vec(),
                status: 200,
                content_type: None,
                headers: HashMap::new(),
                is_json: false,
            });
        }

        // Run before_request hooks
        for hook_name in before_hook_names {
            if let Some(&hook_func) = self.handlers.get(hook_name) {
                let hook_args = PyObjRef::from_owned(ffi::PyTuple_New(1))
                    .ok_or("failed to create hook args")?;
                ffi::Py_INCREF(request_ptr);
                ffi::PyTuple_SetItem(hook_args.as_ptr(), 0, request_ptr);

                let hook_result = PyObjRef::from_owned(ffi::PyObject_Call(
                    hook_func,
                    hook_args.as_ptr(),
                    std::ptr::null_mut(),
                ));

                match hook_result {
                    Some(r) => {
                        // Drive async hooks through the event loop so
                        // `async def` middleware doesn't leak a bare
                        // coroutine object as a "short-circuit response".
                        let resolved = self.resolve_coroutine(r)?;
                        if resolved.as_ptr() != ffi::Py_None() {
                            return self.parse_result(resolved);
                        }
                    }
                    None => {
                        // Hook raised an exception. We previously logged
                        // with PyErr_Print and fell through to the main
                        // handler — a critical bypass for auth / ACL hooks
                        // that signal denial by raising. Return an error
                        // so the caller serves 500 instead of the
                        // unprotected handler output.
                        ffi::PyErr_Print();
                        return Err(format!(
                            "before_request hook {hook_name:?} raised an exception"
                        ));
                    }
                }
            }
        }

        // Call handler(request). We don't own a ref to request_ptr
        // (worker struct does) — pass it through directly.
        let args_arr = [request_ptr];
        let result_obj = PyObjRef::from_owned(ffi::PyObject_Vectorcall(
            func,
            args_arr.as_ptr(),
            1,
            std::ptr::null_mut(),
        ));
        // after_hooks no longer need a separate PyObjRef — the worker
        // retains the request ptr for us. Keep the flag purely for
        // control flow.
        let has_after_hooks = !after_hook_names.is_empty();

        let mut response = match result_obj {
            Some(r) => {
                let resolved = self.resolve_coroutine(r)?;
                self.parse_result(resolved)?
            }
            None => {
                // req_for_hooks dropped here automatically → DECREF
                ffi::PyErr_Print();
                return Err("handler raised an exception".to_string());
            }
        };

        // Run after_request hooks: hook(request, response) → response.
        // Reuses the worker's cached request instance.
        if has_after_hooks {
            for hook_name in after_hook_names {
                if let Some(&hook_func) = self.handlers.get(hook_name) {
                    // Build _PyreResponse from current response
                    let resp_obj = self.build_sky_response(&response)?;

                    let hook_args = PyObjRef::from_owned(ffi::PyTuple_New(2))
                        .ok_or("failed to create hook args")?;
                    ffi::Py_INCREF(request_ptr);
                    ffi::PyTuple_SetItem(hook_args.as_ptr(), 0, request_ptr);
                    ffi::PyTuple_SetItem(hook_args.as_ptr(), 1, resp_obj.into_raw());

                    let hook_result = PyObjRef::from_owned(ffi::PyObject_Call(
                        hook_func,
                        hook_args.as_ptr(),
                        std::ptr::null_mut(),
                    ));

                    match hook_result {
                        Some(r) => {
                            // Drive async after_hooks through the event loop.
                            let resolved = self.resolve_coroutine(r)?;
                            if resolved.as_ptr() != ffi::Py_None() {
                                response = self.parse_result(resolved)?;
                            }
                        }
                        None => {
                            ffi::PyErr_Print();
                        }
                    }
                }
            }

            // _slot_guard (from top of fn) clears at end — no inline
            // cleanup needed here.
        }

        Ok(response)
    }
}

// ---------------------------------------------------------------------------
// Channel-based Interpreter Pool
// ---------------------------------------------------------------------------

pub(crate) struct InterpreterPool {
    /// Dropping senders closes the channel, signaling workers to exit.
    sync_work_tx: crossbeam_channel::Sender<WorkRequest>,
    async_work_tx: Option<crossbeam_channel::Sender<WorkRequest>>,
    /// Worker threads — joined on drop to ensure clean sub-interpreter shutdown.
    worker_threads: Option<Vec<std::thread::JoinHandle<()>>>,
    routers: HashMap<String, Router<usize>>,
    _handler_names: Vec<String>,
    pub(crate) requires_gil: Vec<bool>,
    pub(crate) is_async_handler: Vec<bool>,
    pub(crate) static_dirs: Vec<(String, String)>,
    has_async_workers: bool,
    /// Per-instance CORS configuration (None = disabled).
    pub(crate) cors_config: Option<crate::router::CorsConfig>,
    /// Per-instance request logging flag, shared with worker threads.
    /// Read via Arc clone in worker_thread_loop, not directly from the struct.
    _request_logging: Arc<AtomicBool>,
}

impl Drop for InterpreterPool {
    fn drop(&mut self) {
        // 1. Drop senders to close the channels — workers will exit their recv loop.
        //    (We need to replace them so the Sender::drop fires now, not later.)
        let _ = std::mem::replace(&mut self.sync_work_tx, crossbeam_channel::bounded(0).0);
        let _ = self.async_work_tx.take();

        // 2. Join all worker threads so they finish Py_EndInterpreter BEFORE
        //    the main interpreter tears down (Py_Finalize). Without this join,
        //    workers race against Py_Finalize and segfault.
        //
        // Bounded wait: user handlers can block indefinitely (e.g. a synchronous
        // `requests.get` with no timeout). An unconditional .join() would hang
        // the whole process on shutdown. Give each worker 5s to observe the
        // channel close and run its Py_EndInterpreter cleanup; if it's stuck
        // in user code past that, forget the thread. The process is exiting
        // anyway — the OS reclaims memory. A stuck sub-interp leaks only
        // what hasn't been freed yet, which is strictly better than hanging
        // indefinitely.
        if let Some(threads) = self.worker_threads.take() {
            const JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
            for t in threads {
                // std::thread::JoinHandle has no timed join, so we spin a
                // short poll loop by checking is_finished(). is_finished()
                // is a cheap atomic read.
                let deadline = std::time::Instant::now() + JOIN_TIMEOUT;
                while !t.is_finished() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                if t.is_finished() {
                    let _ = t.join();
                } else {
                    tracing::warn!(
                        target: "pyre::server",
                        "worker thread did not exit within {:?} — abandoning (process shutdown in progress)",
                        JOIN_TIMEOUT,
                    );
                    // Leak the JoinHandle — OS will reclaim at process exit.
                    std::mem::forget(t);
                }
            }
        }
    }
}

unsafe impl Send for InterpreterPool {}
unsafe impl Sync for InterpreterPool {}

impl InterpreterPool {
    /// Create N sub-interpreters, each in its own OS thread, connected via channels.
    ///
    /// Must be called with the main interpreter's GIL held (before `py.detach()`).
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn new(
        n: usize,
        _py: Python<'_>,
        script_path: &str,
        handler_names: &[String],
        routers: HashMap<String, Router<usize>>,
        before_hook_names: &[String],
        after_hook_names: &[String],
        static_dirs: Vec<(String, String)>,
        requires_gil: Vec<bool>,
        is_async_handler: Vec<bool>,
        cors_config: Option<crate::router::CorsConfig>,
        request_logging: bool,
    ) -> Result<Self, String> {
        let has_any_async = is_async_handler.iter().any(|&a| a);
        // Set PYRE_WORKER=1 so user's app.run() becomes a no-op in sub-interpreters.
        // This replaces the fragile AST-based script filtering.
        std::env::set_var("PYRE_WORKER", "1");

        let raw_script = std::fs::read_to_string(script_path)
            .map_err(|e| format!("Failed to read script: {e}"))?;

        // Collect all function names we need
        let mut all_func_names: Vec<String> = handler_names.to_vec();
        all_func_names.extend(before_hook_names.iter().cloned());
        all_func_names.extend(after_hook_names.iter().cloned());
        // Deduplicate
        all_func_names.sort();
        all_func_names.dedup();

        // Create work channels
        // Sync pool: handles def handlers (220k req/s)
        // Async pool: handles async def handlers (133k req/s)
        let (sync_work_tx, sync_work_rx) = crossbeam_channel::bounded::<WorkRequest>(n * 128);
        let (async_work_tx, async_work_rx) = if has_any_async {
            let (tx, rx) = crossbeam_channel::bounded::<WorkRequest>(n * 128);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        // Determine worker split: if async handlers exist, split workers
        let (sync_count, _async_count) = if has_any_async {
            let async_n = (n / 2).max(1).min(n); // At least 1, never exceed total
            (n.saturating_sub(async_n), async_n)
        } else {
            (n, 0)
        };

        // Create sub-interpreters and spawn worker threads
        let mut workers = Vec::new();
        let mut threads = Vec::new();

        for i in 0..n {
            let worker = SubInterpreterWorker::new(&raw_script, script_path, &all_func_names)
                .map_err(|e| format!("sub-interpreter {i}: {e}"))?;
            workers.push(worker);
        }

        // Initialize async worker states if needed
        if has_any_async {
            let async_rx = async_work_rx.as_ref().unwrap();
            let mut states = Vec::with_capacity(n);
            for _ in 0..n {
                states.push(Arc::new(WorkerState {
                    rx: async_rx.clone(),
                    response_map: Mutex::new(HashMap::new()),
                    next_req_id: AtomicU64::new(0),
                }));
            }
            // Overwrite rather than .set() — this pool may not be the
            // first one created in the process (tests / hot-reload).
            // Stale states from a prior pool would cause workers to
            // recv() on closed channels forever.
            if let Ok(mut w) = WORKER_STATES.write() {
                *w = states;
            }
        }

        let logging_flag = Arc::new(AtomicBool::new(request_logging));

        // Spawn workers: first sync_count as sync, rest as async
        for (i, worker) in workers.into_iter().enumerate() {
            let handler_names_clone = handler_names.to_vec();
            let before_hooks_clone = before_hook_names.to_vec();
            let after_hooks_clone = after_hook_names.to_vec();
            let logging = Arc::clone(&logging_flag);

            let handle = if i >= sync_count && has_any_async {
                // Async worker
                std::thread::Builder::new()
                    .name(format!("pyre-async-worker-{i}"))
                    .spawn(move || {
                        worker_thread_loop_async(worker, &handler_names_clone, i);
                    })
                    .map_err(|e| format!("failed to spawn async worker {i}: {e}"))?
            } else {
                // Sync worker
                let rx = sync_work_rx.clone();
                std::thread::Builder::new()
                    .name(format!("pyre-worker-{i}"))
                    .spawn(move || {
                        worker_thread_loop(
                            worker,
                            rx,
                            &handler_names_clone,
                            &before_hooks_clone,
                            &after_hooks_clone,
                            &logging,
                        );
                    })
                    .map_err(|e| format!("failed to spawn worker thread {i}: {e}"))?
            };

            threads.push(handle);
        }

        Ok(InterpreterPool {
            sync_work_tx,
            async_work_tx,
            worker_threads: Some(threads),
            routers,
            _handler_names: handler_names.to_vec(),
            requires_gil,
            is_async_handler: is_async_handler.clone(),
            static_dirs,
            has_async_workers: has_any_async,
            cors_config,
            _request_logging: logging_flag,
        })
    }

    /// Look up a route. Case-insensitive on method per RFC 9110 §9.1 —
    /// matches the sibling `RouteTable::lookup` in src/router.rs. Without
    /// this normalization, lowercase / mixed-case HTTP verbs from the
    /// wire (hyper accepts them) silently fell through to 404 in
    /// sub-interpreter mode.
    pub fn lookup(&self, method: &str, path: &str) -> Option<(usize, Vec<(String, String)>)> {
        let router = if method.bytes().any(|b| b.is_ascii_lowercase()) {
            self.routers.get(&method.to_ascii_uppercase())?
        } else {
            self.routers.get(method)?
        };
        let matched = router.at(path).ok()?;
        // Decode percent-encoded path params — see router.rs for rationale.
        let params: Vec<(String, String)> = matched
            .params
            .iter()
            .map(|(k, v)| {
                let decoded = percent_encoding::percent_decode_str(v)
                    .decode_utf8()
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| v.to_string());
                (k.to_string(), decoded)
            })
            .collect();
        Some((*matched.value, params))
    }

    /// Get handler name by index.
    /// Submit a work request. Routes to sync or async pool based on handler type.
    pub fn submit(&self, req: WorkRequest) -> Result<(), String> {
        // Route to async pool if handler is async and pool exists
        let tx = if self.has_async_workers
            && self
                .is_async_handler
                .get(req.handler_idx)
                .copied()
                .unwrap_or(false)
        {
            self.async_work_tx.as_ref().unwrap()
        } else {
            &self.sync_work_tx
        };

        tx.try_send(req).map_err(|e| match e {
            crossbeam_channel::TrySendError::Full(_) => "server overloaded".to_string(),
            crossbeam_channel::TrySendError::Disconnected(_) => {
                "worker pool channel closed".to_string()
            }
        })
    }
}

/// RAII guard: ensures GIL is released even if a panic occurs mid-handler.
/// Without this, a panic after `PyEval_RestoreThread` but before `PyEval_SaveThread`
/// would leave the GIL permanently locked, causing deadlock on the next request
/// and eventual segfault from corrupted thread state.
///
/// The saved thread state is written back to `tstate_cell` on drop, so the caller
/// can retrieve it even after a panic unwind.
struct SubInterpGilGuard<'a> {
    tstate_cell: &'a std::cell::Cell<*mut ffi::PyThreadState>,
}

impl<'a> SubInterpGilGuard<'a> {
    /// Acquire the sub-interpreter's GIL. On drop, releases it and writes
    /// the saved tstate back to `tstate_cell`.
    unsafe fn acquire(
        tstate: *mut ffi::PyThreadState,
        tstate_cell: &'a std::cell::Cell<*mut ffi::PyThreadState>,
    ) -> Self {
        ffi::PyEval_RestoreThread(tstate);
        Self { tstate_cell }
    }
}

impl Drop for SubInterpGilGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: we always hold the GIL when this guard exists.
        // SaveThread releases it and returns the saved tstate for next acquire.
        unsafe {
            self.tstate_cell.set(ffi::PyEval_SaveThread());
        }
    }
}

/// Main loop for each worker OS thread.
fn worker_thread_loop(
    mut worker: SubInterpreterWorker,
    rx: crossbeam_channel::Receiver<WorkRequest>,
    handler_names: &[String],
    before_hook_names: &[String],
    after_hook_names: &[String],
    request_logging: &AtomicBool,
) {
    while let Ok(req) = rx.recv() {
        // Skip requests whose caller already timed out (504) — avoid wasting
        // CPU on "dead" requests during queue backlog (prevents snowball effect).
        if req.response_tx.is_closed() {
            continue;
        }

        // Cell lives outside catch_unwind so the guard can write tstate back
        // even during panic unwind.
        let tstate_cell = std::cell::Cell::new(worker.tstate);

        // Catch panics to prevent worker thread death.
        // SubInterpGilGuard ensures GIL is released even if call_handler panics.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            let _guard = SubInterpGilGuard::acquire(tstate_cell.get(), &tstate_cell);

            let handler_name = &handler_names[req.handler_idx];
            worker.call_handler(
                handler_name,
                before_hook_names,
                after_hook_names,
                &req.method,
                &req.path,
                &req.params,
                &req.query,
                &req.body,
                &req.headers,
                &req.client_ip,
            )
            // _guard drops here → PyEval_SaveThread() → tstate_cell updated
        }));

        // Recover tstate (updated by guard's Drop, even after panic)
        worker.tstate = tstate_cell.get();

        let response = match result {
            Ok(r) => r,
            Err(_) => Err("internal error: worker panic".to_string()),
        };

        // Log request via tracing (zero-cost when access log is filtered off)
        if request_logging.load(Ordering::Relaxed) {
            let status = match &response {
                Ok(r) => r.status,
                Err(_) => 500,
            };
            if status >= 500 {
                tracing::error!(
                    target: "pyre::access",
                    method = %req.method,
                    path = %req.path,
                    status,
                    "Request failed"
                );
            } else if status >= 400 {
                tracing::warn!(
                    target: "pyre::access",
                    method = %req.method,
                    path = %req.path,
                    status,
                    "Client error"
                );
            } else {
                tracing::info!(
                    target: "pyre::access",
                    method = %req.method,
                    path = %req.path,
                    status,
                    "Request handled"
                );
            }
        }

        // Send response back (ignore error if receiver dropped)
        let _ = req.response_tx.send(response);
        WorkRequest::inc_completed();
    }

    // Channel closed — clean up the sub-interpreter
    unsafe {
        if !worker.tstate.is_null() {
            ffi::PyEval_RestoreThread(worker.tstate);
            ffi::Py_EndInterpreter(ffi::PyThreadState_Get());
            worker.tstate = std::ptr::null_mut();
        }
    }
}

/// Async worker: Python asyncio event loop drives execution.
/// Fetcher thread pulls requests from channel (releasing GIL during wait),
/// asyncio loop runs handlers as concurrent tasks.
fn worker_thread_loop_async(
    mut worker: SubInterpreterWorker,
    handler_names: &[String],
    worker_idx: usize,
) {
    let handlers_array = handler_names
        .iter()
        .map(|n| format!("'{}'", n))
        .collect::<Vec<_>>()
        .join(", ");

    // Load async engine from external Python file (syntax highlighting + maintainability)
    let engine_template = include_str!("../python/pyreframework/_async_engine.py");
    let engine_script =
        format!("WORKER_ID = {worker_idx}\nHANDLER_NAMES = [{handlers_array}]\n{engine_template}");

    unsafe {
        ffi::PyEval_RestoreThread(worker.tstate);

        // Register C-FFI functions in sub-interpreter globals.
        // transmute: PyCFunction (2 args) → PyCFunctionWithKeywords (3 args) —
        // safe because METH_VARARGS ignores the third (kwargs) parameter.
        #[allow(clippy::missing_transmute_annotations)]
        let recv_def = Box::into_raw(Box::new(ffi::PyMethodDef {
            ml_name: c"_pyre_recv".as_ptr(),
            ml_meth: ffi::PyMethodDefPointer {
                PyCFunctionWithKeywords: std::mem::transmute(pyre_recv_cfunc as *const ()),
            },
            ml_flags: ffi::METH_VARARGS,
            ml_doc: std::ptr::null(),
        }));
        #[allow(clippy::missing_transmute_annotations)]
        let send_def = Box::into_raw(Box::new(ffi::PyMethodDef {
            ml_name: c"_pyre_send".as_ptr(),
            ml_meth: ffi::PyMethodDefPointer {
                PyCFunctionWithKeywords: std::mem::transmute(pyre_send_cfunc as *const ()),
            },
            ml_flags: ffi::METH_VARARGS,
            ml_doc: std::ptr::null(),
        }));

        let recv_func =
            ffi::PyCFunction_NewEx(recv_def, std::ptr::null_mut(), std::ptr::null_mut());
        let send_func =
            ffi::PyCFunction_NewEx(send_def, std::ptr::null_mut(), std::ptr::null_mut());

        ffi::PyDict_SetItemString(worker.globals, c"_pyre_recv".as_ptr(), recv_func);
        ffi::PyDict_SetItemString(worker.globals, c"_pyre_send".as_ptr(), send_func);
        ffi::Py_DECREF(recv_func);
        ffi::Py_DECREF(send_func);

        // Register _pyre_emit_log for async worker logging bridge
        #[allow(clippy::missing_transmute_annotations)]
        let emit_log_def = Box::into_raw(Box::new(ffi::PyMethodDef {
            ml_name: c"_pyre_emit_log".as_ptr(),
            ml_meth: ffi::PyMethodDefPointer {
                PyCFunctionWithKeywords: std::mem::transmute(pyre_emit_log_cfunc as *const ()),
            },
            ml_flags: ffi::METH_VARARGS,
            ml_doc: std::ptr::null(),
        }));
        let emit_log_func =
            ffi::PyCFunction_NewEx(emit_log_def, std::ptr::null_mut(), std::ptr::null_mut());
        if !emit_log_func.is_null() {
            ffi::PyDict_SetItemString(worker.globals, c"_pyre_emit_log".as_ptr(), emit_log_func);
            ffi::Py_DECREF(emit_log_func);
        }

        // Run the async engine — this blocks until the channel is closed
        let code = std::ffi::CString::new(engine_script).unwrap();
        let result = ffi::PyRun_String(
            code.as_ptr(),
            ffi::Py_file_input,
            worker.globals,
            worker.globals,
        );
        if result.is_null() {
            ffi::PyErr_Print();
        } else {
            ffi::Py_DECREF(result);
        }

        // Cleanup
        ffi::Py_EndInterpreter(ffi::PyThreadState_Get());
        worker.tstate = std::ptr::null_mut();
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Helper: mint a fresh `Arc<WorkerState>` backed by a dedicated
    /// crossbeam channel so the test has observable state (the rx
    /// handle identifies which install the state came from).
    fn mint_state() -> (Arc<WorkerState>, crossbeam_channel::Sender<WorkRequest>) {
        let (tx, rx) = crossbeam_channel::unbounded::<WorkRequest>();
        let st = Arc::new(WorkerState {
            rx,
            response_map: Mutex::new(HashMap::new()),
            next_req_id: AtomicU64::new(0),
        });
        (st, tx)
    }

    /// Regression for the advisor-flagged hot-reload / test-isolation
    /// bug: the original `OnceLock<Vec<Arc<WorkerState>>>` silently
    /// rejected a second `.set()`, leaving any subsequent
    /// `InterpreterPool::new()` operating on stale channels from the
    /// prior pool — a permanent deadlock on async recv.
    ///
    /// With the fix (`RwLock<Vec<Arc<WorkerState>>>`) a second install
    /// overwrites the first. This test verifies:
    ///   1. First install makes states visible via `get_worker_state`.
    ///   2. Second install REPLACES them — the new Arc identity wins.
    ///   3. The old states are still drop-safe (refcount goes to
    ///      whatever test-scope clones we held; no double-free).
    #[test]
    fn worker_states_can_be_reinstalled() {
        // --- Install #1 --------------------------------------------------
        let (s0_first, _tx0_first) = mint_state();
        let (s1_first, _tx1_first) = mint_state();
        {
            let mut w = WORKER_STATES.write().unwrap();
            *w = vec![s0_first.clone(), s1_first.clone()];
        }

        let got0 = get_worker_state(0).expect("install #1 slot 0 missing");
        let got1 = get_worker_state(1).expect("install #1 slot 1 missing");
        assert!(Arc::ptr_eq(&got0, &s0_first));
        assert!(Arc::ptr_eq(&got1, &s1_first));
        drop(got0);
        drop(got1);

        // --- Install #2 (simulating a second app.run() / hot reload) -----
        let (s0_second, _tx0_second) = mint_state();
        let (s1_second, _tx1_second) = mint_state();
        let (s2_second, _tx2_second) = mint_state();
        {
            let mut w = WORKER_STATES.write().unwrap();
            *w = vec![s0_second.clone(), s1_second.clone(), s2_second.clone()];
        }

        // New identities are visible — the old ones are gone from the
        // registry (though still alive via the test-local `s0_first`
        // etc. clones, which is exactly the invariant we want).
        let got0 = get_worker_state(0).expect("install #2 slot 0 missing");
        let got1 = get_worker_state(1).expect("install #2 slot 1 missing");
        let got2 = get_worker_state(2).expect("install #2 slot 2 missing");
        assert!(Arc::ptr_eq(&got0, &s0_second), "slot 0 still points at pool #1 — OnceLock-style silent failure regression");
        assert!(Arc::ptr_eq(&got1, &s1_second));
        assert!(Arc::ptr_eq(&got2, &s2_second));
        assert!(!Arc::ptr_eq(&got0, &s0_first));
        assert!(!Arc::ptr_eq(&got1, &s1_first));

        // Out-of-range lookup returns None.
        assert!(get_worker_state(99).is_none());

        // Leave the registry empty for the next test in case of global
        // state bleed (RwLock is a static).
        let mut w = WORKER_STATES.write().unwrap();
        w.clear();
    }
}
