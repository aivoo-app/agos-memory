//! OpenAI-compatible providers over the shared HTTP client (issue 0024).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use agos_memory::embed::{Embedder, OpenAiCompatEmbedder};
use agos_memory::http::HttpConfig;
use agos_memory::llm::{ChatClient, OpenAiCompatChat};

/// Tiny single-shot HTTP stub: asserts the bearer token, returns `body`.
fn stub_server(expected_auth: &str, body: &str) -> (String, Arc<Mutex<Option<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let seen = Arc::new(Mutex::new(None));
    let seen2 = seen.clone();
    let auth = expected_auth.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).unwrap_or(0);
        *seen2.lock().unwrap() = Some(String::from_utf8_lossy(&buf[..n]).into_owned());
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes());
    });
    (format!("http://{addr}"), seen)
}

#[tokio::test]
async fn openai_compat_embed_roundtrip() {
    let (base, seen) = stub_server(
        "Bearer tok",
        r#"{"data":[{"index":0,"embedding":[0.1,0.2]},{"index":1,"embedding":[0.3,0.4]}]}"#,
    );
    let e = OpenAiCompatEmbedder::new(HttpConfig::new(&base, "tok", 5), "m", 2);
    let out = e.embed(&["a".to_string(), "b".to_string()]).await.unwrap();
    assert_eq!(out, vec![vec![0.1f32, 0.2], vec![0.3, 0.4]]);
    let req = seen.lock().unwrap().clone().unwrap();
    assert!(req.contains("Bearer tok"), "auth header sent, got: {req}");
    assert!(req.contains("/v1/embeddings"));
}

#[tokio::test]
async fn openai_compat_embed_dim_mismatch_is_typed() {
    let (base, _) = stub_server("Bearer tok", r#"{"data":[{"index":0,"embedding":[0.1]}]}"#);
    let e = OpenAiCompatEmbedder::new(HttpConfig::new(&base, "tok", 5), "m", 2);
    let err = e.embed(&["a".to_string()]).await.unwrap_err();
    assert!(err.to_string().contains("dim mismatch"), "got: {err}");
}

#[tokio::test]
async fn openai_compat_chat_roundtrip() {
    let (base, seen) = stub_server(
        "Bearer tok",
        r#"{"choices":[{"message":{"content":"{\"memories\":[]}"}}]}"#,
    );
    let c = OpenAiCompatChat::new(HttpConfig::new(&base, "tok", 5), "m");
    let out = c.complete("extract this").await.unwrap();
    assert_eq!(out, r#"{"memories":[]}"#);
    let req = seen.lock().unwrap().clone().unwrap();
    assert!(req.contains("Bearer tok"), "auth header sent, got: {req}");
    assert!(req.contains("/v1/chat/completions"));
}
