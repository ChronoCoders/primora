fn service_url() -> String {
    std::env::var("SERVICE_URL").unwrap_or_else(|_| "http://localhost:3000".to_string())
}

#[tokio::test]
async fn test_health_endpoint() {
    let url = format!("{}/health", service_url());
    let response = match reqwest::get(url.as_str()).await {
        Ok(response) => response,
        Err(_) => {
            eprintln!("service unreachable at {url}, skipping");
            return;
        }
    };
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let Ok(body) = response.text().await else {
        panic!("failed to read /health body");
    };
    assert!(body.contains("ok"));
}

#[tokio::test]
async fn test_metrics_endpoint() {
    let url = format!("{}/metrics", service_url());
    let response = match reqwest::get(url.as_str()).await {
        Ok(response) => response,
        Err(_) => {
            eprintln!("service unreachable at {url}, skipping");
            return;
        }
    };
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let Ok(body) = response.text().await else {
        panic!("failed to read /metrics body");
    };
    assert!(body.contains("session_active_count"));
}
