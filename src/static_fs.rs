use bytes::Bytes;
use http_body_util::Full;
use hyper::{Response, StatusCode};

pub fn mime_from_ext(ext: &str) -> &'static str {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_html() {
        assert_eq!(mime_from_ext("html"), "text/html; charset=utf-8");
        assert_eq!(mime_from_ext("htm"), "text/html; charset=utf-8");
    }

    #[test]
    fn mime_js_css() {
        assert_eq!(mime_from_ext("css"), "text/css; charset=utf-8");
        assert_eq!(mime_from_ext("js"), "application/javascript; charset=utf-8");
        assert_eq!(mime_from_ext("mjs"), "application/javascript; charset=utf-8");
    }

    #[test]
    fn mime_images() {
        assert_eq!(mime_from_ext("png"), "image/png");
        assert_eq!(mime_from_ext("jpg"), "image/jpeg");
        assert_eq!(mime_from_ext("jpeg"), "image/jpeg");
        assert_eq!(mime_from_ext("gif"), "image/gif");
        assert_eq!(mime_from_ext("svg"), "image/svg+xml");
        assert_eq!(mime_from_ext("webp"), "image/webp");
        assert_eq!(mime_from_ext("ico"), "image/x-icon");
    }

    #[test]
    fn mime_fonts() {
        assert_eq!(mime_from_ext("woff"), "font/woff");
        assert_eq!(mime_from_ext("woff2"), "font/woff2");
        assert_eq!(mime_from_ext("ttf"), "font/ttf");
        assert_eq!(mime_from_ext("otf"), "font/otf");
    }

    #[test]
    fn mime_application() {
        assert_eq!(mime_from_ext("json"), "application/json; charset=utf-8");
        assert_eq!(mime_from_ext("pdf"), "application/pdf");
        assert_eq!(mime_from_ext("xml"), "application/xml; charset=utf-8");
        assert_eq!(mime_from_ext("wasm"), "application/wasm");
        assert_eq!(mime_from_ext("map"), "application/json");
    }

    #[test]
    fn mime_unknown_fallback() {
        assert_eq!(mime_from_ext("xyz"), "application/octet-stream");
        assert_eq!(mime_from_ext(""), "application/octet-stream");
        assert_eq!(mime_from_ext("bin"), "application/octet-stream");
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
                    .header("server", crate::response::SERVER_HEADER)
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
                    .header("server", crate::response::SERVER_HEADER)
                    .body(Full::new(Bytes::from(contents)))
                    .unwrap(),
            );
        }
    }
    None
}
