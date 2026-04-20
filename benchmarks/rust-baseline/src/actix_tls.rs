//! Actix-web TLS head-to-head baseline vs Pyre.
//!
//! Same /json-fortunes payload, same rustls 0.23 backend (ring crypto),
//! same ALPN (h2 + http/1.1), same ~3 KB JSON. No compression — this
//! isolates pure TLS overhead on top of plain JSON responses, matching
//! the HTTP Arena "JSON TLS" profile shape.
//!
//! Build:
//!     cd benchmarks/rust-baseline && cargo build --release --bin bench-actix-tls
//!
//! Run:
//!     PYRE_TLS_CERT=/path/cert.pem PYRE_TLS_KEY=/path/key.pem \
//!         ./target/release/bench-actix-tls
//!
//! Listens on 127.0.0.1:8443.

use std::fs::File;
use std::io::BufReader;

use actix_web::{get, App, HttpResponse, HttpServer};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use serde_json::json;

fn fortunes() -> serde_json::Value {
    json!({
        "fortunes": [
            {"id": 1, "message": "fortune: No such file or directory"},
            {"id": 2, "message": "A computer scientist is someone who fixes things that aren't broken."},
            {"id": 3, "message": "After enough decimal places, nobody gives a damn."},
            {"id": 4, "message": "A bad random number generator: 1, 1, 1, 1, 1, 4.33e+67, 1, 1, 1"},
            {"id": 5, "message": "A computer program does what you tell it to do, not what you want it to do."},
            {"id": 6, "message": "Emacs is a nice operating system, but I prefer UNIX. — Tom Christaensen"},
            {"id": 7, "message": "Any program that runs right is obsolete."},
            {"id": 8, "message": "A list is only as strong as its weakest link. — Donald Knuth"},
            {"id": 9, "message": "Feature: A bug with seniority."},
            {"id": 10, "message": "Computers make very fast, very accurate mistakes."},
            {"id": 11, "message": "<script>alert(\"This should not be displayed in a browser alert box.\");</script>"},
            {"id": 12, "message": "フレームワークのベンチマーク"},
            {"id": 13, "message": "Additional fortune added at request time."},
            {"id": 14, "message": "Good programmers have a solid grasp of their tools."},
            {"id": 15, "message": "The only constant is change."},
            {"id": 16, "message": "Premature optimization is the root of all evil. — Donald Knuth"},
            {"id": 17, "message": "There are only two hard things in Computer Science: cache invalidation and naming things."},
            {"id": 18, "message": "Testing shows the presence, not the absence of bugs. — Edsger Dijkstra"},
            {"id": 19, "message": "Simplicity is prerequisite for reliability. — Edsger Dijkstra"},
            {"id": 20, "message": "When in doubt, use brute force. — Ken Thompson"},
            {"id": 21, "message": "Controlling complexity is the essence of computer programming. — Brian Kernighan"},
            {"id": 22, "message": "The most important property of a program is whether it accomplishes the intention of its user."},
            {"id": 23, "message": "Measuring programming progress by lines of code is like measuring aircraft building progress by weight."},
            {"id": 24, "message": "The best performance improvement is the transition from the nonworking state to the working state."},
            {"id": 25, "message": "Deleted code is debugged code. — Jeff Sickel"},
            {"id": 26, "message": "First, solve the problem. Then, write the code. — John Johnson"},
            {"id": 27, "message": "Programs must be written for people to read, and only incidentally for machines to execute."},
            {"id": 28, "message": "Any sufficiently advanced bug is indistinguishable from a feature."},
            {"id": 29, "message": "There's no place like 127.0.0.1."},
            {"id": 30, "message": "It is practically impossible to teach good programming to students who have had a prior exposure to BASIC."},
            {"id": 31, "message": "Walking on water and developing software from a specification are easy if both are frozen."},
            {"id": 32, "message": "Debugging is twice as hard as writing the code in the first place."},
        ]
    })
}

#[get("/")]
async fn index() -> HttpResponse {
    HttpResponse::Ok().content_type("text/plain").body("Hello from Actix TLS")
}

#[get("/json-fortunes")]
async fn json_fortunes() -> HttpResponse {
    HttpResponse::Ok().json(fortunes())
}

fn load_tls_config(cert_path: &str, key_path: &str) -> ServerConfig {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cert_file = File::open(cert_path).expect("open cert");
    let mut cert_reader = BufReader::new(cert_file);
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .expect("parse cert");

    let key_file = File::open(key_path).expect("open key");
    let mut key_reader = BufReader::new(key_file);
    let key: PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut key_reader).expect("parse key").expect("no key found");

    let mut cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .expect("ServerConfig build");
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    cfg
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let cert = std::env::var("PYRE_TLS_CERT").expect("PYRE_TLS_CERT required");
    let key = std::env::var("PYRE_TLS_KEY").expect("PYRE_TLS_KEY required");
    let port: u16 = std::env::var("ACTIX_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8443);

    let tls_cfg = load_tls_config(&cert, &key);

    println!(
        "\n  Actix-web TLS baseline listening on https://127.0.0.1:{port}\n  (rustls 0.23 + ring, ALPN: h2 + http/1.1)\n"
    );

    HttpServer::new(|| App::new().service(index).service(json_fortunes))
        .bind_rustls_0_23(("127.0.0.1", port), tls_cfg)?
        .run()
        .await
}
