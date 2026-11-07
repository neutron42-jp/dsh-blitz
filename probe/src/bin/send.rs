use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::path::PathBuf;

fn state_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME").map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share"));
    base.join("dsh-blitz")
}

struct Ctx { origin: String, cookie: String }

impl Ctx {
    fn load() -> Result<Self> {
        Ok(Ctx {
            origin: std::fs::read_to_string(state_dir().join("origin")).map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "http://127.0.0.1:3081".to_string()),
            cookie: std::fs::read_to_string(state_dir().join("cookies")).context("not paired")?.trim().to_string(),
        })
    }
    async fn post(&self, endpoint: &str, args: Value) -> Result<Value> {
        let client = reqwest::Client::new();
        let body = serde_json::json!({"type":"client-request","rpcId":"s1","method":endpoint,"payload":{"args":args}});
        let r = client.post(format!("{}/api/{endpoint}", self.origin))
            .header("Cookie", &self.cookie).header("Content-Type", "application/json").header("Origin", &self.origin)
            .body(body.to_string()).send().await?;
        let status = r.status();
        let text = r.text().await.unwrap_or_default();
        if !status.is_success() { return Err(anyhow!("{endpoint}: HTTP {status}: {text}")); }
        let v: Value = serde_json::from_str(&text)?;
        Ok(v["result"]["value"].clone())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    let ctx = Ctx::load()?;
    let (sid, text) = if argv.get(1).map(|s| s.as_str()) == Some("--new") {
        let text = argv.get(2).cloned().ok_or_else(|| anyhow!("need text"))?;
        let v = ctx.post("session/create", serde_json::json!({"request":{"cwd": std::env::current_dir()?.to_string_lossy()}})).await?;
        (v["sessionId"].as_str().unwrap_or_default().to_string(), text)
    } else {
        (argv.get(1).cloned().ok_or_else(|| anyhow!("usage: send <sessionId|--new> <text>"))?, argv.get(2).cloned().ok_or_else(|| anyhow!("need text"))?)
    };
    let rid = format!("req-{}", std::process::id());
    let v = ctx.post("session/prompt", serde_json::json!({"request":{
        "requestId": rid, "sessionId": sid, "mode": "queue",
        "content": [{"type":"text","text": text}]
    }})).await?;
    println!("{sid} accepted={}", v["accepted"]);
    Ok(())
}
