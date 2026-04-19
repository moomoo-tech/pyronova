//! `_PyreRequest` as a raw C-API heap type.
//!
//! PEP 684 sub-interpreters have a bug where a normal Python-class
//! `tp_dealloc` does not reliably DECREF `__slots__` members —
//! per-request string and bytes values orphan-leak in the sub-interp's
//! PyMalloc arena (~1000 B/request in the hello bench, ~500 MB/s
//! at load).
//!
//! pyo3 0.28 refuses to register `#[pyclass]` types in sub-interpreters
//! (pymodule.rs hard-codes the rejection). So we bypass pyo3 entirely:
//! build the type directly via `PyType_FromSpec` + `PyMemberDef` with
//! a custom `tp_dealloc` that synchronously `Py_XDECREF`s every slot.
//!
//! One type object is created per sub-interpreter (types are per-interp
//! state) and injected into the sub-interp's globals as `_PyreRequest`.
//!
//! **Defensive invariant: NEVER add `Py_TPFLAGS_BASETYPE` to the flags.**
//! If Python can subclass this type, CPython's `subtype_dealloc` takes
//! over and silently bypasses our custom `tp_dealloc` — the whole leak
//! fix becomes a no-op. We learned this the hard way when per-request
//! dealloc count stayed at 0 despite tp_dealloc being wired correctly.
//! Helper methods (`.text()`, `.json()`, `.body`, `.query_params`) are
//! instead monkey-patched onto the heap type at sub-interp init.

use pyo3::ffi;
use std::ffi::{c_char, c_int, c_void};

#[cfg(feature = "leak_detect")]
use std::sync::atomic::{AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// Diagnostics (leak_detect feature only)
// ---------------------------------------------------------------------------
//
// These counters were the probes that isolated the dealloc path during
// the v1.4.5 leak hunt. Kept behind the feature so they remain available
// for future regressions without any cost in the shipped binary.

#[cfg(feature = "leak_detect")]
pub(crate) static DEALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "leak_detect")]
pub(crate) static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "leak_detect")]
pub(crate) static SLOT_RC: [[AtomicUsize; 8]; 7] =
    [const { [const { AtomicUsize::new(0) }; 8] }; 7];

#[cfg(feature = "leak_detect")]
unsafe fn record_slot_rc(ordinal: usize, ptr: *mut ffi::PyObject) {
    if ptr.is_null() {
        return;
    }
    let rc = ffi::Py_REFCNT(ptr);
    let bucket = if rc < 0 {
        0
    } else if rc >= 7 {
        7
    } else {
        rc as usize
    };
    SLOT_RC[ordinal][bucket].fetch_add(1, Ordering::Relaxed);
}

#[cfg(feature = "leak_detect")]
pub fn slot_rc_report() -> String {
    let slots = [
        "method", "path", "params", "query", "body_bytes", "headers", "client_ip",
    ];
    let mut s = String::from("slot rc@dealloc histogram:\n");
    for (i, name) in slots.iter().enumerate() {
        s.push_str(&format!("  {:10}", name));
        for b in 0..8 {
            let n = SLOT_RC[i][b].load(Ordering::Relaxed);
            let label = if b == 7 { "7+".to_string() } else { b.to_string() };
            s.push_str(&format!(" rc{}:{}", label, n));
        }
        s.push('\n');
    }
    s
}

/// C-layout storage backing a `_PyreRequest` instance.
///
/// All seven slot pointers carry a strong reference. They are set
/// non-null by `alloc_and_init` and Py_XDECREF'd by `pyre_request_dealloc`.
#[repr(C)]
pub(crate) struct PyreRequestInner {
    pub ob_base: ffi::PyObject,
    pub method: *mut ffi::PyObject,
    pub path: *mut ffi::PyObject,
    pub params: *mut ffi::PyObject,
    pub query: *mut ffi::PyObject,
    pub body_bytes: *mut ffi::PyObject,
    pub headers: *mut ffi::PyObject,
    pub client_ip: *mut ffi::PyObject,
}

// --- tp_dealloc ---------------------------------------------------------

unsafe extern "C" fn pyre_request_dealloc(obj: *mut ffi::PyObject) {
    #[cfg(feature = "leak_detect")]
    DEALLOC_COUNT.fetch_add(1, Ordering::Relaxed);

    let inner = obj as *mut PyreRequestInner;
    // Take the type BEFORE tp_free — tp_free may invalidate Py_TYPE(obj).
    let tp = ffi::Py_TYPE(obj);

    #[cfg(feature = "leak_detect")]
    {
        // Tally refcount of each slot at dealloc time.
        // rc=1 → freed on DECREF. rc>=2 → someone else holds a ref;
        // we drop to rc-1 and the value orphan-leaks.
        record_slot_rc(0, (*inner).method);
        record_slot_rc(1, (*inner).path);
        record_slot_rc(2, (*inner).params);
        record_slot_rc(3, (*inner).query);
        record_slot_rc(4, (*inner).body_bytes);
        record_slot_rc(5, (*inner).headers);
        record_slot_rc(6, (*inner).client_ip);
    }

    // DECREF each slot. Py_XDECREF is a no-op on NULL, so partial
    // construction (e.g. OOM mid-alloc_and_init) is safe.
    ffi::Py_XDECREF((*inner).method);
    ffi::Py_XDECREF((*inner).path);
    ffi::Py_XDECREF((*inner).params);
    ffi::Py_XDECREF((*inner).query);
    ffi::Py_XDECREF((*inner).body_bytes);
    ffi::Py_XDECREF((*inner).headers);
    ffi::Py_XDECREF((*inner).client_ip);

    // Hand storage back to the type's allocator.
    if let Some(free) = (*tp).tp_free {
        free(obj as *mut c_void);
    } else {
        ffi::PyObject_Free(obj as *mut c_void);
    }
    // Heap types hold a ref to their type object. Release it AFTER
    // tp_free (CPython convention — tp_free may still need the type).
    ffi::Py_DECREF(tp as *mut ffi::PyObject);
}

// --- tp_init ------------------------------------------------------------
//
// Accepts the same 7 positional args the old Python `_PyreRequest.__init__`
// did: (method, path, params, query, body_bytes, headers, client_ip).
// Needed so Python code that constructs `_PyreRequest(...)` directly
// (e.g. the async engine bridge in `_async_engine.py`) keeps working.
// The fast path in Rust `build_request` skips this entirely — it fills
// the struct via `alloc_and_init` with zero Python-side argument
// marshalling — so this init path only pays the cost when user / async
// code explicitly instantiates from Python.

unsafe extern "C" fn pyre_request_init(
    self_: *mut ffi::PyObject,
    args: *mut ffi::PyObject,
    _kwargs: *mut ffi::PyObject,
) -> c_int {
    let inner = self_ as *mut PyreRequestInner;
    let mut method: *mut ffi::PyObject = std::ptr::null_mut();
    let mut path: *mut ffi::PyObject = std::ptr::null_mut();
    let mut params: *mut ffi::PyObject = std::ptr::null_mut();
    let mut query: *mut ffi::PyObject = std::ptr::null_mut();
    let mut body_bytes: *mut ffi::PyObject = std::ptr::null_mut();
    let mut headers: *mut ffi::PyObject = std::ptr::null_mut();
    let mut client_ip: *mut ffi::PyObject = std::ptr::null_mut();
    // "OOOOOOO" — seven borrowed refs. PyArg_ParseTuple does NOT INCREF
    // the targets, so we must bump refcounts ourselves before storing.
    let fmt = c"OOOOOOO";
    if ffi::PyArg_ParseTuple(
        args,
        fmt.as_ptr(),
        &mut method,
        &mut path,
        &mut params,
        &mut query,
        &mut body_bytes,
        &mut headers,
        &mut client_ip,
    ) == 0
    {
        return -1;
    }
    // Release any previous contents (in case __init__ is called twice —
    // CPython's API contract allows this even if unusual).
    ffi::Py_XDECREF((*inner).method);
    ffi::Py_XDECREF((*inner).path);
    ffi::Py_XDECREF((*inner).params);
    ffi::Py_XDECREF((*inner).query);
    ffi::Py_XDECREF((*inner).body_bytes);
    ffi::Py_XDECREF((*inner).headers);
    ffi::Py_XDECREF((*inner).client_ip);
    ffi::Py_INCREF(method);
    ffi::Py_INCREF(path);
    ffi::Py_INCREF(params);
    ffi::Py_INCREF(query);
    ffi::Py_INCREF(body_bytes);
    ffi::Py_INCREF(headers);
    ffi::Py_INCREF(client_ip);
    (*inner).method = method;
    (*inner).path = path;
    (*inner).params = params;
    (*inner).query = query;
    (*inner).body_bytes = body_bytes;
    (*inner).headers = headers;
    (*inner).client_ip = client_ip;
    0
}

// --- PyMemberDef table --------------------------------------------------
//
// Descriptors exposed read-only (flags = READONLY = 1) so user handlers
// can read req.method etc. but cannot replace them — also keeps the GC
// story simple (no SetAttr path to worry about).

const READONLY: c_int = 1; // Py_READONLY
const TYPE_OBJECT_EX: c_int = ffi::Py_T_OBJECT_EX;

static MEMBER_NAME_METHOD: &[u8] = b"method\0";
static MEMBER_NAME_PATH: &[u8] = b"path\0";
static MEMBER_NAME_PARAMS: &[u8] = b"params\0";
static MEMBER_NAME_QUERY: &[u8] = b"query\0";
static MEMBER_NAME_BODY_BYTES: &[u8] = b"body_bytes\0";
static MEMBER_NAME_HEADERS: &[u8] = b"headers\0";
static MEMBER_NAME_CLIENT_IP: &[u8] = b"client_ip\0";

fn members_table() -> [ffi::PyMemberDef; 8] {
    use std::mem::offset_of;
    [
        ffi::PyMemberDef {
            name: MEMBER_NAME_METHOD.as_ptr() as *const c_char,
            type_code: TYPE_OBJECT_EX,
            offset: offset_of!(PyreRequestInner, method) as ffi::Py_ssize_t,
            flags: READONLY,
            doc: std::ptr::null(),
        },
        ffi::PyMemberDef {
            name: MEMBER_NAME_PATH.as_ptr() as *const c_char,
            type_code: TYPE_OBJECT_EX,
            offset: offset_of!(PyreRequestInner, path) as ffi::Py_ssize_t,
            flags: READONLY,
            doc: std::ptr::null(),
        },
        ffi::PyMemberDef {
            name: MEMBER_NAME_PARAMS.as_ptr() as *const c_char,
            type_code: TYPE_OBJECT_EX,
            offset: offset_of!(PyreRequestInner, params) as ffi::Py_ssize_t,
            flags: READONLY,
            doc: std::ptr::null(),
        },
        ffi::PyMemberDef {
            name: MEMBER_NAME_QUERY.as_ptr() as *const c_char,
            type_code: TYPE_OBJECT_EX,
            offset: offset_of!(PyreRequestInner, query) as ffi::Py_ssize_t,
            flags: READONLY,
            doc: std::ptr::null(),
        },
        ffi::PyMemberDef {
            name: MEMBER_NAME_BODY_BYTES.as_ptr() as *const c_char,
            type_code: TYPE_OBJECT_EX,
            offset: offset_of!(PyreRequestInner, body_bytes) as ffi::Py_ssize_t,
            flags: READONLY,
            doc: std::ptr::null(),
        },
        ffi::PyMemberDef {
            name: MEMBER_NAME_HEADERS.as_ptr() as *const c_char,
            type_code: TYPE_OBJECT_EX,
            offset: offset_of!(PyreRequestInner, headers) as ffi::Py_ssize_t,
            flags: READONLY,
            doc: std::ptr::null(),
        },
        ffi::PyMemberDef {
            name: MEMBER_NAME_CLIENT_IP.as_ptr() as *const c_char,
            type_code: TYPE_OBJECT_EX,
            offset: offset_of!(PyreRequestInner, client_ip) as ffi::Py_ssize_t,
            flags: READONLY,
            doc: std::ptr::null(),
        },
        // Null-terminator sentinel.
        ffi::PyMemberDef {
            name: std::ptr::null(),
            type_code: 0,
            offset: 0,
            flags: 0,
            doc: std::ptr::null(),
        },
    ]
}

// --- Type registration --------------------------------------------------

static TYPE_NAME: &[u8] = b"pyreframework._PyreRequest\0";

/// Build the `_PyreRequest` type for the current sub-interpreter.
///
/// Returns a new owned reference to the type object. Caller is expected
/// to install it in the sub-interp's globals under the name
/// `_PyreRequest` (the Python bootstrap script uses this name).
///
/// # Safety
/// Must be called with the current sub-interpreter's GIL held.
pub(crate) unsafe fn register_type() -> Result<*mut ffi::PyObject, String> {
    // Members and slots must live at least as long as the type object.
    // PyType_FromSpec copies the slot array but NOT the Py_tp_members
    // table — the pointer in the slot is retained. So the members array
    // must outlive every instance of the type. We leak a Box per
    // sub-interp (there's a bounded number of them) to guarantee that.
    let members = Box::leak(Box::new(members_table()));

    let slots = Box::leak(Box::new([
        ffi::PyType_Slot {
            slot: ffi::Py_tp_dealloc,
            pfunc: pyre_request_dealloc as *mut c_void,
        },
        ffi::PyType_Slot {
            slot: ffi::Py_tp_init,
            pfunc: pyre_request_init as *mut c_void,
        },
        ffi::PyType_Slot {
            slot: ffi::Py_tp_members,
            pfunc: members.as_mut_ptr() as *mut c_void,
        },
        ffi::PyType_Slot {
            slot: 0,
            pfunc: std::ptr::null_mut(),
        },
    ]));

    let spec = Box::leak(Box::new(ffi::PyType_Spec {
        name: TYPE_NAME.as_ptr() as *const c_char,
        basicsize: std::mem::size_of::<PyreRequestInner>() as c_int,
        itemsize: 0,
        // Intentionally NO `Py_TPFLAGS_BASETYPE` — see module docstring.
        flags: ffi::Py_TPFLAGS_DEFAULT as u32,
        slots: slots.as_mut_ptr(),
    }));

    let ty = ffi::PyType_FromSpec(spec as *mut ffi::PyType_Spec);
    if ty.is_null() {
        return Err("PyType_FromSpec(_PyreRequest) failed".to_string());
    }
    Ok(ty)
}

/// Allocate a fresh `_PyreRequest` instance with the given slot values.
///
/// Each `*mut PyObject` argument is a **new owned reference** whose
/// ownership is transferred into the slot. The caller must NOT DECREF
/// them after this call — the instance's `tp_dealloc` will.
///
/// On failure the arguments are DECREF'd (responsibility transfer was
/// atomic: either the slot owns them, or we dispose of them).
///
/// # Safety
/// * `type_obj` must be the type returned by `register_type` for the
///   CURRENT sub-interpreter.
/// * The current sub-interpreter's GIL must be held.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn alloc_and_init(
    type_obj: *mut ffi::PyObject,
    method: *mut ffi::PyObject,
    path: *mut ffi::PyObject,
    params: *mut ffi::PyObject,
    query: *mut ffi::PyObject,
    body_bytes: *mut ffi::PyObject,
    headers: *mut ffi::PyObject,
    client_ip: *mut ffi::PyObject,
) -> Result<*mut ffi::PyObject, String> {
    #[cfg(feature = "leak_detect")]
    ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);

    let obj = ffi::PyType_GenericAlloc(type_obj as *mut ffi::PyTypeObject, 0);
    if obj.is_null() {
        // Alloc failed — dispose of transferred refs and report.
        ffi::Py_XDECREF(method);
        ffi::Py_XDECREF(path);
        ffi::Py_XDECREF(params);
        ffi::Py_XDECREF(query);
        ffi::Py_XDECREF(body_bytes);
        ffi::Py_XDECREF(headers);
        ffi::Py_XDECREF(client_ip);
        return Err("PyType_GenericAlloc(_PyreRequest) failed".to_string());
    }
    let inner = obj as *mut PyreRequestInner;
    // PyType_GenericAlloc zero-initializes the basicsize region, so
    // the seven pointers are already NULL. Overwrite with owned refs.
    (*inner).method = method;
    (*inner).path = path;
    (*inner).params = params;
    (*inner).query = query;
    (*inner).body_bytes = body_bytes;
    (*inner).headers = headers;
    (*inner).client_ip = client_ip;
    Ok(obj)
}

