//! Catalog-fetcher tests against a canned HTTP mock (no provider
//! network involved). Each fetcher asserts its path, headers, and envelope.
use model_openai_compatible::OpenAICompatibleClient;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

struct Seen {
    path: String,
    headers: HashMap<String, String>,
}

async fn spawn_mock() -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_clone = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let seen = Arc::clone(&seen_clone);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let Ok(n) = stream.read(&mut buf).await else {
                    return;
                };
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let mut lines = head.lines();
                let request_line = lines.next().unwrap_or_default().to_string();
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
                let mut headers = HashMap::new();
                for line in lines {
                    if line.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        headers.insert(k.trim().to_lowercase(), v.trim().to_string());
                    }
                }
                seen.lock().unwrap().push(Seen {
                    path: path.clone(),
                    headers,
                });
                let (status, body): (&str, String) = match path.as_str() {
                    // Wrong envelope on purpose (for the invalid-response test).
                    "/models" => ("200 OK", r#"{"models":[]}"#.to_string()),
                    "/v1/models" => (
                        "200 OK",
                        r#"{"data":[{"id":"a"},{"id":"b"}]}"#.to_string(),
                    ),
                    "/api/v1/models" => ("404 Not Found", "nope".to_string()),
                    "/api/tags" => (
                        "200 OK",
                        r#"{"models":[{"name":"llama3"}]}"#.to_string(),
                    ),
                    "/v1beta/models" => (
                        "200 OK",
                        r#"{"models":[{"name":"models/gem-x","displayName":"Gem X"}]}"#.to_string(),
                    ),
                    "/eleven/models" => (
                        "200 OK",
                        r#"[{"model_id":"eleven_v3","name":"Eleven v3","can_do_text_to_speech":true}]"#
                            .to_string(),
                    ),
                    _ => ("404 Not Found", "nope".to_string()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(body.as_bytes()).await;
            });
        }
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

#[tokio::test]
async fn openai_catalog_uses_bearer_and_data_envelope() {
    let (base, seen) = spawn_mock().await;
    let data = OpenAICompatibleClient::new(format!("{base}/v1"), Some("k".to_string()), "")
        .fetch_models()
        .await
        .unwrap();
    assert_eq!(data["data"].as_array().unwrap().len(), 2);
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].path, "/v1/models");
    assert_eq!(
        seen[0].headers.get("authorization").map(String::as_str),
        Some("Bearer k")
    );
}

#[tokio::test]
async fn anthropic_catalog_uses_key_headers() {
    let (base, seen) = spawn_mock().await;
    let data = OpenAICompatibleClient::new(format!("{base}/v1"), Some("ant".to_string()), "")
        .fetch_anthropic_models()
        .await
        .unwrap();
    assert_eq!(data["data"].as_array().unwrap().len(), 2);
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen[0].headers.get("x-api-key").map(String::as_str),
        Some("ant")
    );
    assert_eq!(
        seen[0].headers.get("anthropic-version").map(String::as_str),
        Some("2023-06-01")
    );
    assert!(!seen[0].headers.contains_key("authorization"));
}

#[tokio::test]
async fn ollama_tags_hit_api_tags() {
    let (base, seen) = spawn_mock().await;
    // A trailing /v1 is stripped: tags live at the server root.
    let data = OpenAICompatibleClient::new(format!("{base}/v1"), None, "")
        .fetch_ollama_tags()
        .await
        .unwrap();
    assert_eq!(data["models"][0]["name"], "llama3");
    assert_eq!(seen.lock().unwrap()[0].path, "/api/tags");
}

#[tokio::test]
async fn lmstudio_falls_back_to_openai_models_on_404() {
    let (base, seen) = spawn_mock().await;
    let data = OpenAICompatibleClient::new(base.clone(), None, "")
        .fetch_lmstudio_models()
        .await
        .unwrap();
    assert_eq!(data["data"].as_array().unwrap().len(), 2);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[0].path, "/api/v1/models");
    assert_eq!(seen[1].path, "/api/v0/models");
    assert_eq!(seen[2].path, "/v1/models");
}

#[tokio::test]
async fn google_and_elevenlabs_envelopes() {
    let (base, _) = spawn_mock().await;
    let data = OpenAICompatibleClient::new(format!("{base}/v1beta"), Some("g".to_string()), "")
        .fetch_google_models()
        .await
        .unwrap();
    assert_eq!(data["models"][0]["name"], "models/gem-x");

    let data = OpenAICompatibleClient::new(format!("{base}/eleven"), Some("e".to_string()), "")
        .fetch_elevenlabs_models()
        .await
        .unwrap();
    assert!(data.as_array().unwrap().len() == 1);
}

#[tokio::test]
async fn wrong_envelope_is_an_invalid_response() {
    let (base, _) = spawn_mock().await;
    // /api/tags returns {models:...}, not {data:...}.
    let err = OpenAICompatibleClient::new(base, None, "")
        .fetch_models()
        .await
        .unwrap_err();
    assert!(err.to_string().contains("invalid"), "{err}");
}
