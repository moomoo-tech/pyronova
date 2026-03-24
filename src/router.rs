use std::collections::HashMap;
use std::sync::Arc;

use matchit::Router;
use parking_lot::RwLock;
use pyo3::prelude::*;

pub(crate) struct RouteTable {
    pub(crate) handlers: Vec<Py<PyAny>>,
    pub(crate) handler_names: Vec<String>,
    pub(crate) requires_gil: Vec<bool>,
    pub(crate) routers: HashMap<String, Router<usize>>,
    pub(crate) ws_handlers: HashMap<String, Py<PyAny>>,
    pub(crate) before_hooks: Vec<Py<PyAny>>,
    pub(crate) after_hooks: Vec<Py<PyAny>>,
    pub(crate) before_hook_names: Vec<String>,
    pub(crate) after_hook_names: Vec<String>,
    pub(crate) fallback_handler: Option<Py<PyAny>>,
    pub(crate) fallback_handler_name: Option<String>,
    pub(crate) static_dirs: Vec<(String, String)>,
}

impl RouteTable {
    pub(crate) fn new() -> Self {
        RouteTable {
            handlers: Vec::new(),
            handler_names: Vec::new(),
            requires_gil: Vec::new(),
            routers: HashMap::new(),
            ws_handlers: HashMap::new(),
            before_hooks: Vec::new(),
            after_hooks: Vec::new(),
            before_hook_names: Vec::new(),
            after_hook_names: Vec::new(),
            fallback_handler: None,
            fallback_handler_name: None,
            static_dirs: Vec::new(),
        }
    }

    pub(crate) fn insert(
        &mut self,
        method: &str,
        path: &str,
        handler: Py<PyAny>,
        handler_name: String,
        gil: bool,
    ) -> Result<(), String> {
        let idx = self.handlers.len();
        self.handlers.push(handler);
        self.handler_names.push(handler_name);
        self.requires_gil.push(gil);
        let router = self
            .routers
            .entry(method.to_uppercase())
            .or_insert_with(Router::new);
        router.insert(path, idx).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub(crate) fn lookup(&self, method: &str, path: &str) -> Option<(usize, HashMap<String, String>)> {
        let router = self.routers.get(method)?;
        let matched = router.at(path).ok()?;
        let params: HashMap<String, String> = matched
            .params
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Some((*matched.value, params))
    }
}

unsafe impl Send for RouteTable {}
unsafe impl Sync for RouteTable {}

pub(crate) type SharedRoutes = Arc<RwLock<RouteTable>>;
