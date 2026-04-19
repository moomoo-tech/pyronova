//! Opt-in PyObject lifecycle diagnostics.
//!
//! Gated behind the `leak_detect` Cargo feature. Compiled out of release
//! builds by default. Use only when chasing a sub-interpreter leak — the
//! workflow that found the v1.4.5 `_PyreRequest` + dict leak.
//!
//! What it gives you:
//!   - A per-(type_name, refcount-at-drop) histogram printed every 500K
//!     drops to stderr. The key insight: if a dict consistently drops
//!     with refcount >= 2, someone is holding a reference past its
//!     expected lifetime — that is the leak.
//!
//! Usage:
//!   maturin develop --release --features leak_detect
//!   python examples/hello.py 2> drops.log &
//!   wrk -t4 -c100 -d10s http://127.0.0.1:8000/
//!   grep "DROP-RC" drops.log | tail -5
//!
//! Interpretation:
//!   - `rc=1` at drop ⇒ this PyObjRef was the only holder. After
//!     `Py_DECREF`, rc=0 and the object is freed. Healthy.
//!   - `rc=2` ⇒ one other owner (usually a field on the containing
//!     instance). After our DECREF, rc=1; the instance's subsequent
//!     dealloc is expected to bring rc→0. Healthy.
//!   - `rc>=3` consistently ⇒ an extra hidden owner. Traces an FFI
//!     refcount bug. This is how we found the `_PyObject_MakeTpCall`
//!     internal-tuple leak in sub-interp mode.
//!
//! The functions are `#[inline(never)]` on purpose — even disabled by
//! cfg, keeping them out of the hot-path icache footprint costs almost
//! nothing, and they are cheap to branch into when the feature is on.

use pyo3::ffi;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

static TOTAL: AtomicU64 = AtomicU64::new(0);
static BUCKETS: OnceLock<Mutex<HashMap<(String, ffi::Py_ssize_t), u64>>> = OnceLock::new();
const REPORT_EVERY: u64 = 500_000;

/// Sample a PyObjRef drop. Records type name + refcount-before-DECREF
/// into a global bucket map. Every 500K drops, dumps the top-N buckets
/// to stderr.
///
/// # Safety
/// Caller must hold a sub-interpreter's GIL; `ptr` must be a valid
/// PyObject the caller owns a reference to.
#[inline(never)]
pub unsafe fn record_drop(ptr: *mut ffi::PyObject) {
    if ptr.is_null() {
        return;
    }
    let rc = ffi::Py_REFCNT(ptr);
    let type_name = {
        let t = ffi::Py_TYPE(ptr);
        if t.is_null() {
            "<null_type>".to_string()
        } else {
            let name_ptr = (*t).tp_name;
            if name_ptr.is_null() {
                "<unnamed>".to_string()
            } else {
                std::ffi::CStr::from_ptr(name_ptr)
                    .to_string_lossy()
                    .into_owned()
            }
        }
    };

    let m = BUCKETS.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let mut g = m.lock().unwrap();
        *g.entry((type_name, rc)).or_insert(0) += 1;
    }

    let n = TOTAL.fetch_add(1, Ordering::Relaxed) + 1;
    if n % REPORT_EVERY == 0 {
        dump_top();
    }
}

/// Print the top-N (type, rc) buckets by count. Called automatically
/// every `REPORT_EVERY` drops; may also be called manually from a
/// shutdown hook if you want a final snapshot.
pub fn dump_top() {
    let Some(m) = BUCKETS.get() else { return };
    let g = m.lock().unwrap();
    let mut v: Vec<_> = g.iter().collect();
    v.sort_by_key(|(_, c)| std::cmp::Reverse(**c));
    let top: Vec<_> = v
        .iter()
        .take(8)
        .map(|((t, rc), c)| format!("{t}@rc={rc}: {c}"))
        .collect();
    eprintln!("[leak_detect] top drop buckets: [{}]", top.join(", "));
}
