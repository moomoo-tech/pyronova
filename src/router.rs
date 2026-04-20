use std::collections::HashMap;
use std::sync::Arc;

use matchit::Router;
use parking_lot::RwLock;
use pyo3::prelude::*;

/// Full CORS configuration. When present, applied to every response —
/// not just OPTIONS preflight — per W3C CORS spec requirements for
/// Access-Control-Allow-Credentials and Access-Control-Expose-Headers.
#[derive(Clone, Default)]
pub(crate) struct CorsConfig {
    pub(crate) origin: String,
    pub(crate) methods: String,
    pub(crate) headers: String,
    pub(crate) expose_headers: Option<String>,
    pub(crate) allow_credentials: bool,
}

pub(crate) struct RouteTable {
    pub(crate) handlers: Vec<Py<PyAny>>,
    pub(crate) handler_names: Vec<String>,
    pub(crate) requires_gil: Vec<bool>,
    pub(crate) is_async: Vec<bool>,
    /// Per-route streaming flag. When true, the accept loop skips the
    /// body collect and attaches a `PyreBodyStream` to the request.
    pub(crate) is_stream: Vec<bool>,
    pub(crate) routers: HashMap<String, Router<usize>>,
    pub(crate) ws_handlers: HashMap<String, Py<PyAny>>,
    pub(crate) before_hooks: Vec<Py<PyAny>>,
    pub(crate) after_hooks: Vec<Py<PyAny>>,
    pub(crate) before_hook_names: Vec<String>,
    pub(crate) after_hook_names: Vec<String>,
    pub(crate) fallback_handler: Option<Py<PyAny>>,
    pub(crate) fallback_handler_name: Option<String>,
    pub(crate) static_dirs: Vec<(String, String)>,
    pub(crate) cors_config: Option<CorsConfig>,
    pub(crate) request_logging: bool,
}

impl RouteTable {
    pub(crate) fn new() -> Self {
        RouteTable {
            handlers: Vec::new(),
            handler_names: Vec::new(),
            requires_gil: Vec::new(),
            is_async: Vec::new(),
            is_stream: Vec::new(),
            routers: HashMap::new(),
            ws_handlers: HashMap::new(),
            before_hooks: Vec::new(),
            after_hooks: Vec::new(),
            before_hook_names: Vec::new(),
            after_hook_names: Vec::new(),
            fallback_handler: None,
            fallback_handler_name: None,
            static_dirs: Vec::new(),
            cors_config: None,
            request_logging: false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn insert(
        &mut self,
        method: &str,
        path: &str,
        handler: Py<PyAny>,
        handler_name: String,
        gil: bool,
        is_async: bool,
        is_stream: bool,
    ) -> Result<(), String> {
        let idx = self.handlers.len();
        self.handlers.push(handler);
        self.handler_names.push(handler_name);
        self.requires_gil.push(gil);
        self.is_async.push(is_async);
        self.is_stream.push(is_stream);
        let router = self.routers.entry(method.to_uppercase()).or_default();
        router.insert(path, idx).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub(crate) fn lookup(
        &self,
        method: &str,
        path: &str,
    ) -> Option<(usize, Vec<(String, String)>)> {
        // `insert` stores methods uppercased; lookup must match — clients
        // sending `get` / `Get` previously silently missed routes even
        // though HTTP methods are case-insensitive per RFC 9110 §9.1.
        //
        // Fast path: hyper hands us canonical (already-uppercase) methods
        // for every standard verb, so the vast majority of requests can
        // reuse `method` without allocation. Only fall back to allocating
        // a normalized copy when we actually see lowercase bytes.
        let router = if method.bytes().any(|b| b.is_ascii_lowercase()) {
            self.routers.get(&method.to_ascii_uppercase())?
        } else {
            self.routers.get(method)?
        };
        let matched = router.at(path).ok()?;
        // Path params from matchit are raw URI segments — percent-encoded.
        // Every web framework's users expect `/user/{name}` + `/user/john%20doe`
        // to yield `name = "john doe"`, not `"john%20doe"`. Decode here so
        // Python handlers don't have to import urllib.parse for every route.
        // Key names are route-template identifiers and are always ASCII;
        // we only decode values.
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
}

unsafe impl Send for RouteTable {}
unsafe impl Sync for RouteTable {}

/// Mutable during registration (before run).
pub(crate) type MutableRoutes = Arc<RwLock<RouteTable>>;

/// Frozen after startup — zero-lock reads on the hot path.
pub(crate) type FrozenRoutes = Arc<RouteTable>;
