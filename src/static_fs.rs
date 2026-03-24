use bytes::Bytes;
use http_body_util::Full;
use hyper::{Response, StatusCode};

pub(crate) fn mime_from_ext(ext: &str) -> &'static str {
    match ext {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "webp" => "image/webp",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "pdf" => "application/pdf",
        "xml" => "application/xml; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        "map" => "application/json",
        _ => "application/octet-stream",
    }
}

pub(crate) async fn try_static_file(
    req_path: &str,
    static_dirs: &[(String, String)],
) -> Option<Response<Full<Bytes>>> {
    for (prefix, directory) in static_dirs {
        if !req_path.starts_with(prefix.as_str()) {
            continue;
        }
        let rel = req_path[prefix.len()..].trim_start_matches('/');
        if rel.is_empty() {
            continue;
        }
        if rel.contains("..") {
            return Some(
                Response::builder()
                    .status(StatusCode::FORBIDDEN)
                    .header("server", "Pyre/0.5.0")
                    .body(Full::new(Bytes::from_static(b"forbidden")))
                    .unwrap(),
            );
        }
        let file_path = std::path::PathBuf::from(directory).join(rel);
        if let Ok(contents) = tokio::fs::read(&file_path).await {
            let ext = file_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            let ct = mime_from_ext(ext);
            return Some(
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", ct)
                    .header("server", "Pyre/0.5.0")
                    .body(Full::new(Bytes::from(contents)))
                    .unwrap(),
            );
        }
    }
    None
}
