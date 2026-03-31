//! Safe abstractions for CPython sub-interpreter management.
//!
//! Provides RAII wrappers over raw `pyo3::ffi` pointers to prevent
//! reference count leaks and ensure proper sub-interpreter cleanup.
//! Also implements a channel-based worker pool for true load balancing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

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
/// Uses Vec for O(1) access (no lock on hot path after init).
static WORKER_STATES: OnceLock<Vec<Arc<WorkerState>>> = OnceLock::new();

fn get_worker_state(worker_id: usize) -> Option<Arc<WorkerState>> {
    WORKER_STATES.get().and_then(|v| v.get(worker_id).cloned())
}

// ---------------------------------------------------------------------------
// C-FFI bridge functions for async engine
// ---------------------------------------------------------------------------

/// pyre_recv(worker_id) → (req_id, handler_idx, method, path, query, body) or None
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
            state
                .response_map
                .lock()
                .unwrap()
                .insert(req_id, req.response_tx);

            let tuple = ffi::PyTuple_New(7);
            ffi::PyTuple_SetItem(tuple, 0, ffi::PyLong_FromUnsignedLongLong(req_id));
            ffi::PyTuple_SetItem(
                tuple,
                1,
                ffi::PyLong_FromUnsignedLongLong(req.handler_idx as u64),
            );
            ffi::PyTuple_SetItem(
                tuple,
                2,
                ffi::PyUnicode_FromStringAndSize(
                    req.method.as_ptr() as *const _,
                    req.method.len() as isize,
                ),
            );
            ffi::PyTuple_SetItem(
                tuple,
                3,
                ffi::PyUnicode_FromStringAndSize(
                    req.path.as_ptr() as *const _,
                    req.path.len() as isize,
                ),
            );
            ffi::PyTuple_SetItem(
                tuple,
                4,
                ffi::PyUnicode_FromStringAndSize(
                    req.query.as_ptr() as *const _,
                    req.query.len() as isize,
                ),
            );
            ffi::PyTuple_SetItem(
                tuple,
                5,
                ffi::PyBytes_FromStringAndSize(
                    req.body.as_ptr() as *const _,
                    req.body.len() as isize,
                ),
            );
            // Serialize headers as JSON string for Python
            let headers_json =
                serde_json::to_string(&req.headers).unwrap_or_else(|_| "{}".to_string());
            ffi::PyTuple_SetItem(
                tuple,
                6,
                ffi::PyUnicode_FromStringAndSize(
                    headers_json.as_ptr() as *const _,
                    headers_json.len() as isize,
                ),
            );
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

    // n=isize, K=u64, H=u16, s=str, y#=bytes+len
    if ffi::PyArg_ParseTuple(
        args,
        c"nKHsy#".as_ptr(),
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
        if let Some(tx) = state.response_map.lock().unwrap().remove(&req_id) {
            let resp = SubInterpResponse {
                body,
                status,
                content_type: ctype,
                headers: HashMap::new(),
                is_json: false,
            };
            let _ = tx.send(Ok(resp));
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
        c"ssssin".as_ptr(),
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
        std::ffi::CStr::from_ptr(msg_ptr)
            .to_str()
            .unwrap_or("")
    } else {
        ""
    };
    let pathname = if !path_ptr.is_null() {
        std::ffi::CStr::from_ptr(path_ptr)
            .to_str()
            .unwrap_or("")
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
                // SAFETY: Py_DECREF requires the GIL to be held.
                // This assertion catches accidental drops from non-GIL threads
                // (e.g. Tokio async tasks) during development.
                debug_assert!(
                    ffi::PyGILState_Check() == 1,
                    "PyObjRef dropped without holding the GIL — this is a use-after-free bug"
                );
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
pub(crate) unsafe fn py_str_dict(map: &HashMap<String, String>) -> Option<PyObjRef> {
    let dict = PyObjRef::from_owned(ffi::PyDict_New())?;
    for (k, v) in map {
        let pk = py_str(k)?;
        let pv = py_str(v)?;
        ffi::PyDict_SetItem(dict.as_ptr(), pk.as_ptr(), pv.as_ptr());
        // pk and pv are dropped here, DECREF'd automatically
    }
    Some(dict)
}

/// Extract a Rust String from a Python str object (raw FFI).
pub(crate) unsafe fn pyobj_to_string(obj: *mut ffi::PyObject) -> Result<String, String> {
    let mut size: isize = 0;
    let ptr = ffi::PyUnicode_AsUTF8AndSize(obj, &mut size);
    if ptr.is_null() {
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
    pub params: HashMap<String, String>,
    pub query: String,
    pub body: Vec<u8>,
    pub headers: HashMap<String, String>,
    pub response_tx: tokio::sync::oneshot::Sender<Result<SubInterpResponse, String>>,
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
    /// Cached: _PyreRequest class pointer
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

        // We are now in the sub-interpreter's thread state.
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
                PyCFunctionWithKeywords: std::mem::transmute(
                    pyre_emit_log_cfunc as *const (),
                ),
            },
            ml_flags: ffi::METH_VARARGS,
            ml_doc: std::ptr::null(),
        }));
        let emit_log_func =
            ffi::PyCFunction_NewEx(emit_log_def, std::ptr::null_mut(), std::ptr::null_mut());
        if !emit_log_func.is_null() {
            ffi::PyDict_SetItemString(
                globals.as_ptr(),
                c"_pyre_emit_log".as_ptr(),
                emit_log_func,
            );
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
            // globals dropped here → DECREF
            ffi::PyThreadState_Swap(main_tstate);
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

        // Cache frequently-used Python objects to avoid per-request lookups
        let req_cls_name = std::ffi::CString::new("_PyreRequest").unwrap();
        let sky_request_cls = ffi::PyDict_GetItemString(globals.as_ptr(), req_cls_name.as_ptr());
        if !sky_request_cls.is_null() {
            ffi::Py_INCREF(sky_request_cls);
        }

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

        // Release this sub-interpreter's GIL
        let saved = ffi::PyEval_SaveThread();

        // Switch back to main interpreter
        ffi::PyThreadState_Swap(main_tstate);

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

    /// Build a _PyreRequest Python object in this sub-interpreter.
    ///
    /// # Safety
    /// Must be called with this sub-interpreter's GIL held.
    unsafe fn build_request(
        &self,
        method: &str,
        path: &str,
        params: &HashMap<String, String>,
        query: &str,
        body: &[u8],
        headers: &HashMap<String, String>,
    ) -> Result<PyObjRef, String> {
        let py_method = py_str(method).ok_or("failed to create py_method")?;
        let py_path = py_str(path).ok_or("failed to create py_path")?;
        let py_params = py_str_dict(params).ok_or("failed to create py_params")?;
        let py_query = py_str(query).ok_or("failed to create py_query")?;
        let py_body = py_bytes(body).ok_or("failed to create py_body")?;
        let py_headers = py_str_dict(headers).ok_or("failed to create py_headers")?;

        // Build args tuple — PyTuple_SetItem steals references
        let args =
            PyObjRef::from_owned(ffi::PyTuple_New(6)).ok_or("failed to create args tuple")?;
        ffi::PyTuple_SetItem(args.as_ptr(), 0, py_method.into_raw());
        ffi::PyTuple_SetItem(args.as_ptr(), 1, py_path.into_raw());
        ffi::PyTuple_SetItem(args.as_ptr(), 2, py_params.into_raw());
        ffi::PyTuple_SetItem(args.as_ptr(), 3, py_query.into_raw());
        ffi::PyTuple_SetItem(args.as_ptr(), 4, py_body.into_raw());
        ffi::PyTuple_SetItem(args.as_ptr(), 5, py_headers.into_raw());

        // Use cached _PyreRequest class
        let req_cls = self.sky_request_cls;
        if req_cls.is_null() {
            return Err("_PyreRequest class not found".to_string());
        }

        let request_obj = PyObjRef::from_owned(ffi::PyObject_Call(
            req_cls,
            args.as_ptr(),
            std::ptr::null_mut(),
        ));
        // args dropped here → DECREF

        request_obj.ok_or_else(|| {
            ffi::PyErr_Print();
            "failed to create _PyreRequest object".to_string()
        })
    }

    /// Parse a handler return value into SubInterpResponse.
    ///
    /// # Safety
    /// Must be called with this sub-interpreter's GIL held.
    unsafe fn parse_result(&self, result_obj: PyObjRef) -> Result<SubInterpResponse, String> {
        let ptr = result_obj.as_ptr();

        // Check if it's a _PyreResponse or any response-like object
        // (duck typing: has status_code + body attributes)
        let resp_cls = self.sky_response_cls;
        let is_response = if !resp_cls.is_null() && ffi::PyObject_IsInstance(ptr, resp_cls) == 1 {
            true
        } else {
            // Duck-type check: has status_code attribute?
            let has_status = ffi::PyObject_HasAttrString(ptr, c"status_code".as_ptr()) == 1;
            let has_body = ffi::PyObject_HasAttrString(ptr, c"body".as_ptr()) == 1;
            has_status && has_body
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
        let str_obj = PyObjRef::from_owned(ffi::PyObject_Str(ptr)).ok_or("str() failed")?;
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

        // Convert body Vec<u8> to Python str (for _PyreResponse)
        let body_str = String::from_utf8_lossy(&resp.body);
        let py_body = py_str(&body_str).ok_or("failed to create body str")?;
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

    /// If obj is a coroutine (async def), execute it via persistent event loop.
    /// Otherwise return it unchanged.
    unsafe fn resolve_coroutine(&self, obj: PyObjRef) -> Result<PyObjRef, String> {
        if ffi::PyCoro_CheckExact(obj.as_ptr()) != 1 {
            return Ok(obj); // Not a coroutine, pass through
        }
        if self.loop_run_func.is_null() {
            return Err("async handler used but asyncio event loop not available".to_string());
        }
        // Call loop.run_until_complete(coroutine)
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
                Some(a) => ffi::PyLong_AsLong(a.as_ptr()) as u16,
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
        let mut resp_headers = HashMap::new();
        {
            let attr = PyObjRef::from_owned(ffi::PyObject_GetAttrString(ptr, c"headers".as_ptr()));
            if let Some(a) = &attr {
                if ffi::PyDict_Check(a.as_ptr()) != 0 {
                    let mut pos: isize = 0;
                    let mut key: *mut ffi::PyObject = std::ptr::null_mut();
                    let mut val: *mut ffi::PyObject = std::ptr::null_mut();
                    while ffi::PyDict_Next(a.as_ptr(), &mut pos, &mut key, &mut val) != 0 {
                        // Coerce both key and value to str via PyObject_Str
                        let str_key = PyObjRef::from_owned(ffi::PyObject_Str(key));
                        let str_val = PyObjRef::from_owned(ffi::PyObject_Str(val));
                        if let (Some(sk), Some(sv)) = (str_key, str_val) {
                            if let (Ok(k), Ok(v)) =
                                (pyobj_to_string(sk.as_ptr()), pyobj_to_string(sv.as_ptr()))
                            {
                                resp_headers.insert(k, v);
                            }
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
                            Err(_) => (Vec::new(), false),
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
        &self,
        handler_name: &str,
        before_hook_names: &[String],
        after_hook_names: &[String],
        method: &str,
        path: &str,
        params: &HashMap<String, String>,
        query: &str,
        body: &[u8],
        headers: &HashMap<String, String>,
    ) -> Result<SubInterpResponse, String> {
        let func = *self
            .handlers
            .get(handler_name)
            .ok_or_else(|| format!("handler '{}' not found", handler_name))?;

        let request_obj = self.build_request(method, path, params, query, body, headers)?;

        // Run before_request hooks
        for hook_name in before_hook_names {
            if let Some(&hook_func) = self.handlers.get(hook_name) {
                let hook_args = PyObjRef::from_owned(ffi::PyTuple_New(1))
                    .ok_or("failed to create hook args")?;
                ffi::Py_INCREF(request_obj.as_ptr());
                ffi::PyTuple_SetItem(hook_args.as_ptr(), 0, request_obj.as_ptr());

                let hook_result = PyObjRef::from_owned(ffi::PyObject_Call(
                    hook_func,
                    hook_args.as_ptr(),
                    std::ptr::null_mut(),
                ));

                match hook_result {
                    Some(r) if r.as_ptr() != ffi::Py_None() => {
                        // Short-circuit
                        return self.parse_result(r);
                    }
                    None => {
                        ffi::PyErr_Print();
                    }
                    _ => {}
                }
            }
        }

        // Call handler(request)
        let call_args =
            PyObjRef::from_owned(ffi::PyTuple_New(1)).ok_or("failed to create call args")?;
        ffi::PyTuple_SetItem(call_args.as_ptr(), 0, request_obj.into_raw());

        let result_obj = PyObjRef::from_owned(ffi::PyObject_Call(
            func,
            call_args.as_ptr(),
            std::ptr::null_mut(),
        ));
        // call_args dropped → DECREF

        let mut response = match result_obj {
            Some(r) => {
                let resolved = self.resolve_coroutine(r)?;
                self.parse_result(resolved)?
            }
            None => {
                ffi::PyErr_Print();
                return Err("handler raised an exception".to_string());
            }
        };

        // Run after_request hooks: hook(request, response) → response
        if !after_hook_names.is_empty() {
            // Rebuild request object for hooks (reuse the original params)
            let req_for_hooks = self.build_request(method, path, params, query, body, headers)?;

            for hook_name in after_hook_names {
                if let Some(&hook_func) = self.handlers.get(hook_name) {
                    // Build _PyreResponse from current response
                    let resp_obj = self.build_sky_response(&response)?;

                    let hook_args = PyObjRef::from_owned(ffi::PyTuple_New(2))
                        .ok_or("failed to create hook args")?;
                    ffi::Py_INCREF(req_for_hooks.as_ptr());
                    ffi::PyTuple_SetItem(hook_args.as_ptr(), 0, req_for_hooks.as_ptr());
                    ffi::PyTuple_SetItem(hook_args.as_ptr(), 1, resp_obj.into_raw());

                    let hook_result = PyObjRef::from_owned(ffi::PyObject_Call(
                        hook_func,
                        hook_args.as_ptr(),
                        std::ptr::null_mut(),
                    ));

                    match hook_result {
                        Some(r) if r.as_ptr() != ffi::Py_None() => {
                            response = self.parse_result(r)?;
                        }
                        None => {
                            ffi::PyErr_Print();
                        }
                        _ => {}
                    }
                }
            }
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
    /// Per-instance CORS origin (None = disabled).
    pub(crate) cors_origin: Option<String>,
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
        if let Some(threads) = self.worker_threads.take() {
            for t in threads {
                let _ = t.join();
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
        cors_origin: Option<String>,
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
            let async_n = (n / 2).max(2); // At least 2 async workers
            (n - async_n, async_n)
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
            let _ = WORKER_STATES.set(states);
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
            cors_origin,
            _request_logging: logging_flag,
        })
    }

    /// Look up a route.
    pub fn lookup(&self, method: &str, path: &str) -> Option<(usize, HashMap<String, String>)> {
        let router = self.routers.get(method)?;
        let matched = router.at(path).ok()?;
        let params: HashMap<String, String> = matched
            .params
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
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
                PyCFunctionWithKeywords: std::mem::transmute(
                    pyre_emit_log_cfunc as *const (),
                ),
            },
            ml_flags: ffi::METH_VARARGS,
            ml_doc: std::ptr::null(),
        }));
        let emit_log_func =
            ffi::PyCFunction_NewEx(emit_log_def, std::ptr::null_mut(), std::ptr::null_mut());
        if !emit_log_func.is_null() {
            ffi::PyDict_SetItemString(
                worker.globals,
                c"_pyre_emit_log".as_ptr(),
                emit_log_func,
            );
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
