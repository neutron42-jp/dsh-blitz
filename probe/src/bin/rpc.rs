use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::path::PathBuf;

fn state_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME").map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share"));
    base.join("dsh-blitz")
}

#[tokio::main]
async fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    let endpoint = argv.get(1).ok_or_else(|| anyhow!("usage: rpc <endpoint> [json-args]"))?.clone();
    let args: Value = argv.get(2).map(|s| serde_json::from_str(s).unwrap_or(Value::Null)).unwrap_or(serde_json::json!({}));

    let origin = std::fs::read_to_string(state_dir().join("origin")).map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "http://127.0.0.1:3081".to_string());
    let cookie = std::fs::read_to_string(state_dir().join("cookies")).context("not paired")?;

    let body = serde_json::json!({
        "type": "client-request",
        "rpcId": "rpc-1",
        "method": endpoint,
        "payload": { "args": args },
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{origin}/api/{endpoint}"))
        .header("Cookie", cookie.trim())
        .header("Content-Type", "application/json")
        .header("Origin", &origin)
        .body(body.to_string())
        .send()
        .await
        .context("post")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    println!("HTTP {status}");
    let limit: usize = std::env::var("RPC_LIMIT").ok().and_then(|v| v.parse().ok()).unwrap_or(1200);
    println!("{}", &text[..text.len().min(limit)]);
    Ok(())
}
