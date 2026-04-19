use bytes::Bytes;
use http_body_util::Full;
use hyper::{Response, StatusCode};
use tokio::io::AsyncReadExt;

/// Maximum size (in bytes) of a static file served out of memory.
/// Files larger than this are refused with 413 to avoid OOM on pathological
/// requests (multi-GB files in the static dir, etc.).
const MAX_STATIC_FILE_BYTES: u64 = 16 * 1024 * 1024; // 16 MiB

/// Return the MIME type for a file extension. The extension is expected in
/// lowercase without a leading dot (e.g. `mime_from_ext("png")`). Falls
/// back to `application/octet-stream` for unknown extensions.
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

        // Cheap fast-reject for obvious traversal in the untrusted request.
        // Not the only defense — the canonicalize check below is what
        // actually keeps us honest against symlinks and encoded variants.
        if rel.contains("..") {
            return Some(forbidden_response());
        }

        let candidate = std::path::PathBuf::from(directory).join(rel);

        // Resolve both the configured root and the candidate file to their
        // canonical absolute forms. This follows symlinks and collapses any
        // remaining `..` segments. An attacker who plants a symlink inside
        // `directory` pointing to `/etc/passwd` will have their access
        // blocked by the `starts_with` containment check below.
        let root_canonical = match tokio::fs::canonicalize(directory).await {
            Ok(p) => p,
            Err(_) => continue, // misconfigured root: try the next mount
        };
        let file_canonical = match tokio::fs::canonicalize(&candidate).await {
            Ok(p) => p,
            Err(_) => continue, // non-existent file: let routing fall through
        };
        if !file_canonical.starts_with(&root_canonical) {
            return Some(forbidden_response());
        }

        // Open once and derive metadata from the fd. Previously we called
        // tokio::fs::metadata(path) and then tokio::fs::read(path) in two
        // separate syscalls — a TOCTOU race where an attacker swapping
        // the file between the two calls could bypass the size limit. By
        // opening first and calling metadata() on the File, both checks
        // operate on the same inode; symlink or rename swaps after open
        // cannot change which bytes we read.
        //
        // O_NOFOLLOW: the canonicalize containment check above followed
        // symlinks to resolve the target. Between that and the open, an
        // attacker with write access to the final path segment could
        // swap the file for a symlink pointing anywhere on disk. With
        // O_NOFOLLOW, `open` refuses to follow a symlink at the last
        // component and returns ELOOP — closing the TOCTOU window.
        // Legitimate symlinks inside the static root are resolved by
        // canonicalize above; we only refuse symlinks that *appeared*
        // after the containment decision was made.
        let file = {
            #[cfg(unix)]
            {
                let mut opts = tokio::fs::OpenOptions::new();
                opts.read(true).custom_flags(libc::O_NOFOLLOW);
                match opts.open(&file_canonical).await {
                    Ok(f) => f,
                    Err(_) => continue,
                }
            }
            #[cfg(not(unix))]
            {
                match tokio::fs::File::open(&file_canonical).await {
                    Ok(f) => f,
                    Err(_) => continue,
                }
            }
        };
        let metadata = match file.metadata().await {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !metadata.is_file() {
            continue;
        }
        if metadata.len() > MAX_STATIC_FILE_BYTES {
            return Some(payload_too_large_response());
        }

        // Belt + braces: even if the metadata-reported size was stale for
        // any reason, `take()` enforces the byte cap on the read itself.
        let cap = metadata.len().min(MAX_STATIC_FILE_BYTES);
        let mut contents = Vec::with_capacity(cap as usize);
        if file
            .take(MAX_STATIC_FILE_BYTES)
            .read_to_end(&mut contents)
            .await
            .is_err()
        {
            continue;
        }
        let ext = file_canonical
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let ct = mime_from_ext(ext);
        return Some(ok_response(ct, contents));
    }
    None
}

// ─── Response builders ──────────────────────────────────────────────
//
// The three helpers below build static responses from constant header
// values + a fixed-size body. They cannot fail in practice (every input
// is known-valid), so `.expect` is used instead of bubbling a Result.
// `nosniff` is added to every response to prevent MIME-type sniffing
// attacks when users upload content into the static directory.

fn forbidden_response() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header("server", crate::response::SERVER_HEADER)
        .header("x-content-type-options", "nosniff")
        .body(Full::new(Bytes::from_static(b"forbidden")))
        .expect("static forbidden response: constant components are valid")
}

fn payload_too_large_response() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::PAYLOAD_TOO_LARGE)
        .header("server", crate::response::SERVER_HEADER)
        .header("x-content-type-options", "nosniff")
        .body(Full::new(Bytes::from_static(b"payload too large")))
        .expect("static payload-too-large response: constant components are valid")
}

fn ok_response(content_type: &'static str, contents: Vec<u8>) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", content_type)
        .header("server", crate::response::SERVER_HEADER)
        .header("x-content-type-options", "nosniff")
        .body(Full::new(Bytes::from(contents)))
        .expect("static ok response: constant headers + validated ct are valid")
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
        assert_eq!(
            mime_from_ext("mjs"),
            "application/javascript; charset=utf-8"
        );
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

    // ── Security / path-traversal tests ────────────────────────────

    async fn write_file(p: &std::path::Path, bytes: &[u8]) {
        tokio::fs::write(p, bytes).await.expect("test setup: write");
    }

    #[tokio::test]
    async fn serves_a_file_inside_the_static_root() {
        let tmp = tempdir();
        let root = tmp.path();
        write_file(&root.join("ok.txt"), b"hello").await;

        let dirs = vec![("/static".to_string(), root.to_string_lossy().to_string())];
        let resp = try_static_file("/static/ok.txt", &dirs).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("x-content-type-options").unwrap(),
            "nosniff"
        );
    }

    #[tokio::test]
    async fn rejects_symlink_escape() {
        let tmp = tempdir();
        let root = tmp.path();
        // A secret lives OUTSIDE the static root.
        let secret_dir = tempdir();
        write_file(&secret_dir.path().join("secret.txt"), b"SHHH").await;

        // Attacker plants a symlink inside root pointing at the secret.
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            secret_dir.path().join("secret.txt"),
            root.join("escape.txt"),
        )
        .unwrap();

        let dirs = vec![("/static".to_string(), root.to_string_lossy().to_string())];
        let resp = try_static_file("/static/escape.txt", &dirs).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn rejects_parent_path_literal() {
        let tmp = tempdir();
        let dirs = vec![(
            "/static".to_string(),
            tmp.path().to_string_lossy().to_string(),
        )];
        let resp = try_static_file("/static/../etc/passwd", &dirs)
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn refuses_oversized_files() {
        let tmp = tempdir();
        let big = tmp.path().join("big.bin");
        let big_len = MAX_STATIC_FILE_BYTES + 1;
        {
            let file = std::fs::File::create(&big).unwrap();
            file.set_len(big_len).unwrap();
            // `file` drops here, ensuring metadata is flushed before the
            // async try_static_file reads it.
        }

        let dirs = vec![(
            "/static".to_string(),
            tmp.path().to_string_lossy().to_string(),
        )];
        let resp = try_static_file("/static/big.bin", &dirs).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn non_existent_file_returns_none() {
        let tmp = tempdir();
        let dirs = vec![(
            "/static".to_string(),
            tmp.path().to_string_lossy().to_string(),
        )];
        assert!(try_static_file("/static/missing.txt", &dirs)
            .await
            .is_none());
    }

    // Minimal tempdir helper: avoid pulling in a dep just for tests.
    // Uses a process-global atomic counter (not a clock) so two calls in
    // rapid succession are guaranteed distinct paths.
    use std::sync::atomic::{AtomicU64, Ordering};
    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tempdir() -> TempDir {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut base = std::env::temp_dir();
        base.push(format!(
            "pyre-static-fs-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&base); // clean stale from prior crashed run
        std::fs::create_dir_all(&base).unwrap();
        TempDir { path: base }
    }
    struct TempDir {
        path: std::path::PathBuf,
    }
    impl TempDir {
        fn path(&self) -> &std::path::Path {
            &self.path
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}
