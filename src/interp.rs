//! Safe abstractions for CPython sub-interpreter management.
//!
//! Provides RAII wrappers over raw `pyo3::ffi` pointers to prevent
//! reference count leaks and ensure proper sub-interpreter cleanup.
//! Also implements a channel-based worker pool for true load balancing.

use std::collections::HashMap;
use std::sync::Arc;

use matchit::Router;
use pyo3::ffi;
use pyo3::prelude::*;

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
// AST-based script filtering
// ---------------------------------------------------------------------------

/// Filter a user script using Python's `ast` module to remove framework code.
///
/// Removes:
/// - `from skytrade import ...` / `import skytrade`
/// - `app = SkyApp(...)` / `app = Pyre(...)`
/// - `app.get(...)`, `app.post(...)`, etc.
/// - `if __name__ == "__main__":` blocks
///
/// Must be called with the main interpreter's GIL held.
pub(crate) fn filter_script_ast(py: Python<'_>, source: &str) -> PyResult<String> {
    let ast_filter_code = r#"
def _pyre_filter_script(source):
    import ast
    try:
        tree = ast.parse(source)
    except SyntaxError:
        return source  # If parse fails, return as-is

    new_body = []
    for node in tree.body:
        # Skip: from skytrade import ... / import skytrade
        if isinstance(node, ast.ImportFrom):
            if node.module and node.module.startswith('skytrade'):
                continue
        if isinstance(node, ast.Import):
            if any(alias.name.startswith('skytrade') for alias in node.names):
                continue

        # Skip: app = SkyApp(...) / app = Pyre(...)
        skip = False
        if isinstance(node, ast.Assign):
            for target in node.targets:
                if isinstance(target, ast.Name) and target.id == 'app':
                    skip = True
                    break
        if skip:
            continue

        # Skip: app.get(...), app.post(...), app.route(...), etc.
        if isinstance(node, ast.Expr) and isinstance(node.value, ast.Call):
            func = node.value.func
            if isinstance(func, ast.Attribute) and isinstance(func.value, ast.Name):
                if func.value.id == 'app':
                    continue

        # Skip: if __name__ == "__main__":
        if isinstance(node, ast.If):
            test = node.test
            if isinstance(test, ast.Compare):
                if isinstance(test.left, ast.Name) and test.left.id == '__name__':
                    continue

        # Skip: decorated functions where decorator is app.get etc.
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            has_app_decorator = False
            new_decorators = []
            for dec in node.decorator_list:
                if isinstance(dec, ast.Call) and isinstance(dec.func, ast.Attribute):
                    if isinstance(dec.func.value, ast.Name) and dec.func.value.id == 'app':
                        has_app_decorator = True
                        continue
                elif isinstance(dec, ast.Attribute):
                    if isinstance(dec.value, ast.Name) and dec.value.id == 'app':
                        has_app_decorator = True
                        continue
                new_decorators.append(dec)
            node.decorator_list = new_decorators

        new_body.append(node)

    tree.body = new_body
    return ast.unparse(tree)
"#;

    // Run the filter function
    let globals = py.import("__main__")?.dict();
    let code_cstr = std::ffi::CString::new(ast_filter_code)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
    py.run(code_cstr.as_c_str(), Some(&globals), None)?;
    let filter_fn = globals.get_item("_pyre_filter_script")?.ok_or_else(|| {
        pyo3::exceptions::PyRuntimeError::new_err("AST filter function not found")
    })?;
    let result = filter_fn.call1((source,))?;
    result.extract::<String>()
}

// ---------------------------------------------------------------------------
// Sub-interpreter response
// ---------------------------------------------------------------------------

/// Result from a sub-interpreter handler call.
pub(crate) struct SubInterpResponse {
    pub body: String,
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
}

unsafe impl Send for SubInterpreterWorker {}

impl SubInterpreterWorker {
    /// Create a new sub-interpreter, execute the filtered script, extract handlers.
    ///
    /// # Safety
    /// Must be called while the main interpreter's thread state is current.
    /// Switches to the new sub-interpreter and back to main on completion.
    unsafe fn new(
        script: &str,
        script_path: &str,
        func_names: &[String],
    ) -> Result<Self, String> {
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
        // Run the bootstrap + filtered user script.
        let bootstrap = format!(
            r#"
class _SkyRequest:
    def __init__(self, method, path, params, query, body_bytes, headers):
        self.method = method
        self.path = path
        self.params = params
        self.query = query
        self.body_bytes = body_bytes
        self.headers = headers
    @property
    def body(self):
        return self.body_bytes
    @property
    def query_params(self):
        from urllib.parse import parse_qs
        return {{k: v[0] for k, v in parse_qs(self.query).items()}}
    def text(self):
        return self.body_bytes.decode('utf-8') if isinstance(self.body_bytes, bytes) else str(self.body_bytes)
    def json(self):
        import json
        return json.loads(self.text())

class _SkyResponse:
    def __init__(self, body="", status_code=200, content_type=None, headers=None):
        self.body = body
        self.status_code = status_code
        self.content_type = content_type
        self.headers = headers or {{}}

# User code (AST-filtered)
{}
"#,
            script
        );

        let globals = PyObjRef::from_owned(ffi::PyDict_New())
            .ok_or("failed to create globals dict")?;
        let builtins = ffi::PyEval_GetBuiltins(); // borrowed ref
        ffi::PyDict_SetItemString(globals.as_ptr(), c"__builtins__".as_ptr(), builtins);

        let code_cstr = std::ffi::CString::new(bootstrap.as_bytes())
            .map_err(|e| format!("CString error: {e}"))?;
        let _filename_cstr = std::ffi::CString::new(script_path)
            .map_err(|e| format!("CString error: {e}"))?;

        let result = PyObjRef::from_owned(ffi::PyRun_String(
            code_cstr.as_ptr(),
            ffi::Py_file_input.try_into().unwrap(),
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
        })
    }

    /// Build a _SkyRequest Python object in this sub-interpreter.
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
        let args = PyObjRef::from_owned(ffi::PyTuple_New(6))
            .ok_or("failed to create args tuple")?;
        ffi::PyTuple_SetItem(args.as_ptr(), 0, py_method.into_raw());
        ffi::PyTuple_SetItem(args.as_ptr(), 1, py_path.into_raw());
        ffi::PyTuple_SetItem(args.as_ptr(), 2, py_params.into_raw());
        ffi::PyTuple_SetItem(args.as_ptr(), 3, py_query.into_raw());
        ffi::PyTuple_SetItem(args.as_ptr(), 4, py_body.into_raw());
        ffi::PyTuple_SetItem(args.as_ptr(), 5, py_headers.into_raw());

        // Get _SkyRequest class
        let req_class_name = std::ffi::CString::new("_SkyRequest").unwrap();
        let req_cls = ffi::PyDict_GetItemString(self.globals, req_class_name.as_ptr());
        if req_cls.is_null() {
            return Err("_SkyRequest class not found".to_string());
        }

        let request_obj = PyObjRef::from_owned(
            ffi::PyObject_Call(req_cls, args.as_ptr(), std::ptr::null_mut()),
        );
        // args dropped here → DECREF

        request_obj.ok_or_else(|| {
            ffi::PyErr_Print();
            "failed to create _SkyRequest object".to_string()
        })
    }

    /// Parse a handler return value into SubInterpResponse.
    ///
    /// # Safety
    /// Must be called with this sub-interpreter's GIL held.
    unsafe fn parse_result(
        &self,
        result_obj: PyObjRef,
    ) -> Result<SubInterpResponse, String> {
        let ptr = result_obj.as_ptr();

        // Check if it's a _SkyResponse
        let resp_class_name = std::ffi::CString::new("_SkyResponse").unwrap();
        let resp_cls = ffi::PyDict_GetItemString(self.globals, resp_class_name.as_ptr());
        if !resp_cls.is_null() && ffi::PyObject_IsInstance(ptr, resp_cls) == 1 {
            return self.parse_sky_response(result_obj);
        }

        // dict → JSON
        if ffi::PyDict_Check(ptr) != 0 {
            let json_str = self.json_dumps(result_obj)?;
            return Ok(SubInterpResponse {
                body: json_str,
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
                body: s,
                status: 200,
                content_type: None,
                headers: HashMap::new(),
                is_json: false,
            });
        }

        // fallback: str(result)
        let str_obj = PyObjRef::from_owned(ffi::PyObject_Str(ptr))
            .ok_or("str() failed")?;
        let s = pyobj_to_string(str_obj.as_ptr())?;
        Ok(SubInterpResponse {
            body: s,
            status: 200,
            content_type: None,
            headers: HashMap::new(),
            is_json: false,
        })
    }

    /// Serialize a Python dict/list to JSON string via json.dumps.
    unsafe fn json_dumps(&self, obj: PyObjRef) -> Result<String, String> {
        let json_mod = PyObjRef::from_owned(ffi::PyImport_ImportModule(c"json".as_ptr()))
            .ok_or("failed to import json")?;
        let dumps = PyObjRef::from_owned(
            ffi::PyObject_GetAttrString(json_mod.as_ptr(), c"dumps".as_ptr()),
        )
        .ok_or("json.dumps not found")?;

        let args = PyObjRef::from_owned(ffi::PyTuple_New(1))
            .ok_or("failed to create tuple")?;
        ffi::PyTuple_SetItem(args.as_ptr(), 0, obj.into_raw());

        let json_str = PyObjRef::from_owned(
            ffi::PyObject_Call(dumps.as_ptr(), args.as_ptr(), std::ptr::null_mut()),
        )
        .ok_or_else(|| {
            ffi::PyErr_Print();
            "json.dumps failed".to_string()
        })?;

        pyobj_to_string(json_str.as_ptr())
    }

    /// Parse a _SkyResponse Python object.
    unsafe fn parse_sky_response(
        &self,
        obj: PyObjRef,
    ) -> Result<SubInterpResponse, String> {
        let ptr = obj.as_ptr();

        // status_code
        let status = {
            let attr = PyObjRef::from_owned(
                ffi::PyObject_GetAttrString(ptr, c"status_code".as_ptr()),
            );
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
            let attr = PyObjRef::from_owned(
                ffi::PyObject_GetAttrString(ptr, c"content_type".as_ptr()),
            );
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
            let attr = PyObjRef::from_owned(
                ffi::PyObject_GetAttrString(ptr, c"headers".as_ptr()),
            );
            if let Some(a) = &attr {
                if ffi::PyDict_Check(a.as_ptr()) != 0 {
                    let mut pos: isize = 0;
                    let mut key: *mut ffi::PyObject = std::ptr::null_mut();
                    let mut val: *mut ffi::PyObject = std::ptr::null_mut();
                    while ffi::PyDict_Next(a.as_ptr(), &mut pos, &mut key, &mut val) != 0 {
                        if let (Ok(k), Ok(v)) = (pyobj_to_string(key), pyobj_to_string(val)) {
                            resp_headers.insert(k, v);
                        }
                    }
                }
            }
            ffi::PyErr_Clear();
        }

        // body
        let (body, is_json) = {
            let attr = PyObjRef::from_owned(
                ffi::PyObject_GetAttrString(ptr, c"body".as_ptr()),
            );
            match attr {
                Some(a) => {
                    if ffi::PyDict_Check(a.as_ptr()) != 0 {
                        match self.json_dumps(a) {
                            Ok(s) => (s, true),
                            Err(_) => (String::new(), false),
                        }
                    } else if ffi::PyUnicode_Check(a.as_ptr()) != 0 {
                        (pyobj_to_string(a.as_ptr()).unwrap_or_default(), false)
                    } else {
                        let str_obj = PyObjRef::from_owned(ffi::PyObject_Str(a.as_ptr()));
                        match str_obj {
                            Some(s) => (pyobj_to_string(s.as_ptr()).unwrap_or_default(), false),
                            None => {
                                ffi::PyErr_Clear();
                                (String::new(), false)
                            }
                        }
                    }
                }
                None => {
                    ffi::PyErr_Clear();
                    (String::new(), false)
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
    unsafe fn call_handler(
        &self,
        handler_name: &str,
        before_hook_names: &[String],
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
        let call_args = PyObjRef::from_owned(ffi::PyTuple_New(1))
            .ok_or("failed to create call args")?;
        ffi::PyTuple_SetItem(call_args.as_ptr(), 0, request_obj.into_raw());

        let result_obj = PyObjRef::from_owned(ffi::PyObject_Call(
            func,
            call_args.as_ptr(),
            std::ptr::null_mut(),
        ));
        // call_args dropped → DECREF

        match result_obj {
            Some(r) => self.parse_result(r),
            None => {
                ffi::PyErr_Print();
                Err("handler raised an exception".to_string())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Channel-based Interpreter Pool
// ---------------------------------------------------------------------------

pub(crate) struct InterpreterPool {
    work_tx: crossbeam_channel::Sender<WorkRequest>,
    _worker_threads: Vec<std::thread::JoinHandle<()>>,
    routers: HashMap<String, Router<usize>>,
    handler_names: Vec<String>,
    pub(crate) requires_gil: Vec<bool>,
    pub(crate) static_dirs: Vec<(String, String)>,
}

unsafe impl Send for InterpreterPool {}
unsafe impl Sync for InterpreterPool {}

impl InterpreterPool {
    /// Create N sub-interpreters, each in its own OS thread, connected via channels.
    ///
    /// Must be called with the main interpreter's GIL held (before `py.detach()`).
    pub unsafe fn new(
        n: usize,
        py: Python<'_>,
        script_path: &str,
        handler_names: &[String],
        routers: HashMap<String, Router<usize>>,
        before_hook_names: &[String],
        static_dirs: Vec<(String, String)>,
        requires_gil: Vec<bool>,
    ) -> Result<Self, String> {
        // Read and filter the script using AST (on the main interpreter)
        let raw_script = std::fs::read_to_string(script_path)
            .map_err(|e| format!("Failed to read script: {e}"))?;

        let filtered_script = filter_script_ast(py, &raw_script)
            .map_err(|e| format!("AST filter error: {e}"))?;

        // Collect all function names we need
        let mut all_func_names: Vec<String> = handler_names.to_vec();
        all_func_names.extend(before_hook_names.iter().cloned());
        // Deduplicate
        all_func_names.sort();
        all_func_names.dedup();

        // Create the shared work channel (multi-producer, multi-consumer)
        let (work_tx, work_rx) = crossbeam_channel::unbounded::<WorkRequest>();

        // Create N sub-interpreters and spawn worker threads
        let mut workers = Vec::new();
        let mut threads = Vec::new();

        for i in 0..n {
            let worker = SubInterpreterWorker::new(
                &filtered_script,
                script_path,
                &all_func_names,
            )
            .map_err(|e| format!("sub-interpreter {i}: {e}"))?;
            workers.push(worker);
        }

        // Spawn OS threads — each owns a SubInterpreterWorker
        for (i, worker) in workers.into_iter().enumerate() {
            let rx = work_rx.clone();
            let handler_names_clone = handler_names.to_vec();
            let before_hooks_clone = before_hook_names.to_vec();

            let handle = std::thread::Builder::new()
                .name(format!("pyre-worker-{i}"))
                .spawn(move || {
                    worker_thread_loop(worker, rx, &handler_names_clone, &before_hooks_clone);
                })
                .map_err(|e| format!("failed to spawn worker thread {i}: {e}"))?;

            threads.push(handle);
        }

        Ok(InterpreterPool {
            work_tx,
            _worker_threads: threads,
            routers,
            handler_names: handler_names.to_vec(),
            requires_gil,
            static_dirs,
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
    pub fn handler_name(&self, idx: usize) -> &str {
        &self.handler_names[idx]
    }

    /// Submit a work request. Returns a oneshot receiver for the response.
    pub fn submit(
        &self,
        req: WorkRequest,
    ) -> Result<(), String> {
        self.work_tx
            .send(req)
            .map_err(|_| "worker pool channel closed".to_string())
    }
}

/// Main loop for each worker OS thread.
fn worker_thread_loop(
    mut worker: SubInterpreterWorker,
    rx: crossbeam_channel::Receiver<WorkRequest>,
    handler_names: &[String],
    before_hook_names: &[String],
) {
    while let Ok(req) = rx.recv() {
        let result = unsafe {
            // Acquire this sub-interpreter's GIL
            ffi::PyEval_RestoreThread(worker.tstate);

            let handler_name = &handler_names[req.handler_idx];
            let result = worker.call_handler(
                handler_name,
                before_hook_names,
                &req.method,
                &req.path,
                &req.params,
                &req.query,
                &req.body,
                &req.headers,
            );

            // Release GIL
            worker.tstate = ffi::PyEval_SaveThread();

            result
        };

        // Send response back (ignore error if receiver dropped)
        let _ = req.response_tx.send(result);
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
