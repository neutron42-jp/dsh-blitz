use anyhow::{anyhow, Context, Result};
use anyrender_vello_cpu::VelloCpuWindowRenderer;
use blitz_dom::DocumentConfig;
use blitz_html::HtmlDocument;
use blitz_shell::{create_default_event_loop, BlitzShellProxy, WindowConfig};
use blitz_traits::shell::{ColorScheme, Viewport};
use futures_util::{SinkExt, StreamExt};
use reqwest::cookie::CookieStore;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use winit::application::ApplicationHandler;
use winit::cursor::{Cursor, CursorIcon};
use winit::dpi::LogicalSize;
use winit::event::{ElementState, Ime, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::keyboard::{Key, NamedKey};
use winit::window::WindowAttributes;

const DEFAULT_ORIGIN: &str = "http://127.0.0.1:3081";
/// Captured DSH shell + icon sprite, resolved from the crate directory.
/// Where the rendered page is mirrored for the shot/boxdump helpers.
fn out_page() -> String {
    asset("live.html")
}

fn asset(name: &str) -> String {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap_or(manifest).join("assets").join(name).display().to_string()
}

const LAYOUT_FIX_CSS: &str = r#"
html, body { height: 100% !important; overflow: hidden !important; }
.pI_x6G_frame { height: 100vh !important; min-height: 0 !important; }
.pI_x6G_centerCol, .pI_x6G_mainPanel { min-height: 0 !important; }
.wSkVaW_root { height: 100% !important; min-height: 0 !important; }
.wSkVaW_body { min-height: 0 !important; }
.wSkVaW_scrollBody { min-height: 0 !important; }
#conv-scroll { flex: 1 1 auto !important; min-height: 0 !important; overflow-y: scroll !important; overflow-x: hidden !important; }
.wSkVaW_body { display: flex !important; flex-direction: column !important; height: 100% !important; min-height: 0 !important; }
.wSkVaW_composerSeat { flex: 0 0 auto !important; position: relative !important; z-index: 9; }
/* Entrance animations freeze at their 0% keyframe (opacity 0) without an
   animation clock, which would hide the whole sidebar. */
.hHd-Xa_wide, .bhn1Oq_wide, .hHd-Xa_regionArea, .hHd-Xa_footArea { animation: none !important; }
#conv-scroll .EvIC1a_frame,
#conv-scroll .EvIC1a_root,
#conv-scroll .EvIC1a_scroll { flex: 0 0 auto !important; height: auto !important; min-height: 0 !important; overflow: hidden !important; }
"#;

extern "C" {
    fn malloc_trim(pad: usize) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
}

/// glibc keeps one arena per thread by default; a UI process with dozens of
/// worker threads pays for that in RSS.
fn limit_malloc_arenas() {
    // M_ARENA_MAX
    unsafe { mallopt(-8, 2) };
}

#[allow(dead_code)]
fn rss_mb() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| l.split_whitespace().nth(1).map(|v| v.to_string()))
        })
        .and_then(|v| v.parse::<f64>().ok())
        .map(|kb| kb / 1024.0)
        .unwrap_or(0.0)
}

/// Single-threaded runtime: these helpers only drive a few sockets, and a
/// multi-threaded runtime would add a worker thread per core to the process.
fn rt_current() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()
}

fn state_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share"));
    base.join("dsh-blitz")
}

fn load_origin() -> String {
    std::fs::read_to_string(state_dir().join("origin"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| DEFAULT_ORIGIN.into())
}

fn load_cookie() -> Result<String> {
    std::fs::read_to_string(state_dir().join("cookies")).map_err(|e| anyhow!("not paired: {e}"))
}

async fn pair_url(url: &str) -> Result<(String, String)> {
    let jar = Arc::new(reqwest::cookie::Jar::default());
    let client = reqwest::Client::builder().cookie_provider(jar.clone()).build()?;
    let resp = client.get(url).send().await.context("pairing")?;
    let status = resp.status();
    let _ = resp.bytes().await;
    if !status.is_success() { return Err(anyhow!("pairing failed: HTTP {status}")); }
    let parsed = reqwest::Url::parse(url)?;
    let origin = format!("{}://{}", parsed.scheme(), match parsed.port() {
        Some(p) => format!("{}:{}", parsed.host_str().unwrap_or("127.0.0.1"), p),
        None => parsed.host_str().unwrap_or("127.0.0.1").to_string(),
    });
    let ourl = reqwest::Url::parse(&origin)?;
    let header = jar.cookies(&ourl).ok_or_else(|| anyhow!("no auth cookie issued"))?;
    let cookie = header.to_str()?.to_string();
    std::fs::create_dir_all(state_dir())?;
    std::fs::write(state_dir().join("cookies"), &cookie)?;
    std::fs::write(state_dir().join("origin"), &origin)?;
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(state_dir().join("cookies"), std::fs::Permissions::from_mode(0o600));
    }
    Ok((cookie, origin))
}

fn open_frame(stream: &str, endpoint: &str, args: Value) -> String {
    serde_json::json!({ "type": "open", "streamId": stream, "endpoint": endpoint, "payload": { "args": args } }).to_string()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn text_of(message: &Value) -> String {
    let mut out = String::new();
    if let Some(arr) = message.get("content").and_then(|c| c.as_array()) {
        for part in arr {
            if let Some(t) = part.get("text").and_then(|t| t.as_str()) { out.push_str(t); }
        }
    }
    out
}

fn is_wide(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF |
        0xA000..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE6F | 0xFF00..=0xFF60 |
        0xFFE0..=0xFFE6 | 0x1F300..=0x1F64F | 0x1F900..=0x1F9FF | 0x20000..=0x3FFFD)
}

fn clamp_caret(text: &str, caret: usize) -> usize {
    if caret >= text.len() { return text.len(); }
    let mut c = caret;
    while c > 0 && !text.is_char_boundary(c) { c -= 1; }
    c
}

fn summary_line(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_string();
    if line.chars().count() > 140 { line.chars().take(140).collect::<String>() + "…" } else { line }
}

fn summary_of(name: &str, args: &str) -> String {
    let v: Value = serde_json::from_str(args).unwrap_or(Value::Null);
    for key in ["description", "command", "file_path", "path", "query"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            let line = s.lines().next().unwrap_or("").trim();
            return line.chars().take(70).collect();
        }
    }
    name.to_string()
}

// ---------------------------------------------------------------- markdown

struct ListNode {
    ordered: bool,
    items: Vec<ListItem>,
}

struct ListItem {
    text: String,
    children: Vec<ListNode>,
}

fn list_marker(line: &str) -> Option<(usize, bool, &str)> {
    let indent = line.len() - line.trim_start().len();
    let rest = &line[indent..];
    for m in ["- ", "* ", "+ "] {
        if let Some(t) = rest.strip_prefix(m) {
            return Some((indent, false, t));
        }
    }
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits > 0 {
        let after = &rest[digits..];
        if let Some(t) = after.strip_prefix(". ").or_else(|| after.strip_prefix(") ")) {
            return Some((indent, true, t));
        }
    }
    None
}

fn heading(line: &str) -> Option<(usize, &str)> {
    let hashes = line.len() - line.trim_start_matches('#').len();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &line[hashes..];
    if !rest.starts_with(' ') {
        return None;
    }
    Some((hashes, rest.trim()))
}

fn is_hr(line: &str) -> bool {
    let t = line.trim();
    if t.len() < 3 {
        return false;
    }
    let c = t.chars().next().unwrap_or(' ');
    if c != '-' && c != '*' && c != '_' {
        return false;
    }
    t.chars().all(|x| x == c || x == ' ')
}

fn table_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_string()).collect()
}

fn is_table_sep(line: &str) -> bool {
    let t = line.trim();
    t.contains('-') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

fn is_block_start(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("\u{60}\u{60}\u{60}")
        || t.starts_with('|')
        || t.starts_with('>')
        || heading(t).is_some()
        || is_hr(t)
        || list_marker(line).is_some()
}

/// Resolve nested inline markers that are already HTML-escaped.
fn inline_spans(escaped: &str) -> String {
    let chars: Vec<char> = escaped.chars().collect();
    let mut out = String::with_capacity(escaped.len() + 16);
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '\u{60}' {
            if let Some(end) = (i + 1..chars.len()).find(|&j| chars[j] == '\u{60}') {
                let code: String = chars[i + 1..end].iter().collect();
                out.push_str("<code>");
                out.push_str(&code);
                out.push_str("</code>");
                i = end + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Local paths have to become file URLs before the renderer can load them.
fn asset_url(src: &str) -> String {
    let s = src.trim();
    if s.starts_with("http://") || s.starts_with("https://") || s.starts_with("data:") || s.starts_with("file://") {
        return s.to_string();
    }
    if s.starts_with('/') {
        return format!("file://{s}");
    }
    let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
    format!("file://{cwd}/{s}")
}

fn inline(text: &str) -> String {
    let escaped = esc(text);
    let chars: Vec<char> = escaped.chars().collect();
    let mut out = String::with_capacity(escaped.len() + 16);
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c == '!' && i + 1 < chars.len() && chars[i + 1] == '[' {
            if let Some(close) = (i + 2..chars.len()).find(|&j| chars[j] == ']') {
                if close + 1 < chars.len() && chars[close + 1] == '(' {
                    if let Some(paren) = (close + 2..chars.len()).find(|&j| chars[j] == ')') {
                        let alt: String = chars[i + 2..close].iter().collect();
                        let src: String = chars[close + 2..paren].iter().collect();
                        out.push_str(&format!(
                            r##"<img class="_image_rve47_300" src="{}" alt="{}" loading="lazy">"##,
                            asset_url(&src),
                            alt
                        ));
                        i = paren + 1;
                        continue;
                    }
                }
            }
        }
        if c == '\u{60}' {
            if let Some(end) = (i + 1..chars.len()).find(|&j| chars[j] == '\u{60}') {
                let code: String = chars[i + 1..end].iter().collect();
                out.push_str("<code>");
                out.push_str(&code);
                out.push_str("</code>");
                i = end + 1;
                continue;
            }
        }
        if c == '*' {
            let double = i + 1 < chars.len() && chars[i + 1] == '*';
            let marker = if double { 2 } else { 1 };
            let mut j = i + marker;
            let mut found = None;
            while j < chars.len() {
                if chars[j] == '*' {
                    if double {
                        if j + 1 < chars.len() && chars[j + 1] == '*' {
                            found = Some(j);
                            break;
                        }
                        j += 1;
                        continue;
                    }
                    found = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(end) = found {
                if end > i + marker {
                    let inner: String = chars[i + marker..end].iter().collect();
                    out.push_str(if double { "<strong>" } else { "<em>" });
                    out.push_str(&inline_spans(&inner));
                    out.push_str(if double { "</strong>" } else { "</em>" });
                    i = end + marker;
                    continue;
                }
            }
        }
        if c == '[' {
            if let Some(close) = (i + 1..chars.len()).find(|&j| chars[j] == ']') {
                if close + 1 < chars.len() && chars[close + 1] == '(' {
                    if let Some(paren) = (close + 2..chars.len()).find(|&j| chars[j] == ')') {
                        let label: String = chars[i + 1..close].iter().collect();
                        let url: String = chars[close + 2..paren].iter().collect();
                        out.push_str("<a href=\"");
                        out.push_str(&url);
                        out.push_str("\">");
                        out.push_str(&label);
                        out.push_str("</a>");
                        i = paren + 1;
                        continue;
                    }
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

fn render_list(node: &ListNode, out: &mut String) {
    out.push_str(if node.ordered { "<ol>" } else { "<ul>" });
    for item in &node.items {
        out.push_str("<li>");
        out.push_str(&inline(&item.text));
        for child in &item.children {
            render_list(child, out);
        }
        out.push_str("</li>");
    }
    out.push_str(if node.ordered { "</ol>" } else { "</ul>" });
}

fn render_markdown(text: &str) -> String {
    let src: Vec<&str> = text.lines().collect();
    let mut out = String::with_capacity(text.len() * 2);
    let mut i = 0usize;
    while i < src.len() {
        let line = src[i];
        let trimmed = line.trim_start();
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        if let Some(lang) = trimmed.strip_prefix("\u{60}\u{60}\u{60}") {
            let lang = lang.trim();
            let mut body = String::new();
            i += 1;
            while i < src.len() && !src[i].trim_start().starts_with("\u{60}\u{60}\u{60}") {
                body.push_str(src[i]);
                body.push('\u{a}');
                i += 1;
            }
            i += 1;
            let class = if lang.is_empty() {
                String::new()
            } else {
                format!(" class=\"language-{}\"", esc(lang))
            };
            out.push_str(&format!("<pre><code{class}>{}</code></pre>", esc(&body)));
            continue;
        }
        if let Some((level, rest)) = heading(trimmed) {
            out.push_str(&format!("<h{level}>{}</h{level}>", inline(rest)));
            i += 1;
            continue;
        }
        if is_hr(trimmed) {
            out.push_str("<hr>");
            i += 1;
            continue;
        }
        if trimmed.starts_with('|') && i + 1 < src.len() && is_table_sep(src[i + 1]) {
            let head = table_cells(trimmed);
            i += 2;
            let mut rows: Vec<Vec<String>> = Vec::new();
            while i < src.len() && src[i].trim_start().starts_with('|') {
                rows.push(table_cells(src[i]));
                i += 1;
            }
            out.push_str("<div class=\"_tableScroll_rve47_199 _tableFill_rve47_252\"><table><thead><tr>");
            for cell in &head {
                out.push_str(&format!("<th>{}</th>", inline(cell)));
            }
            out.push_str("</tr></thead><tbody>");
            for row in &rows {
                out.push_str("<tr>");
                for cell in row {
                    out.push_str(&format!("<td>{}</td>", inline(cell)));
                }
                out.push_str("</tr>");
            }
            out.push_str("</tbody></table></div>");
            continue;
        }
        if trimmed.starts_with('>') {
            let mut body = String::new();
            while i < src.len() && src[i].trim_start().starts_with('>') {
                body.push_str(src[i].trim_start().trim_start_matches('>').trim_start());
                body.push('\u{a}');
                i += 1;
            }
            out.push_str("<blockquote>");
            out.push_str(&render_markdown(&body));
            out.push_str("</blockquote>");
            continue;
        }
        if list_marker(line).is_some() {
            let mut stack: Vec<(usize, ListNode)> = Vec::new();
            let mut root: Option<ListNode> = None;
            while i < src.len() {
                let current = src[i];
                if current.trim().is_empty() {
                    // A blank line continues the list only when the next item
                    // stays at the same depth and marker kind.
                    let mut next = i;
                    while next < src.len() && src[next].trim().is_empty() {
                        next += 1;
                    }
                    let cont = match (src.get(next).and_then(|l| list_marker(l)), stack.last()) {
                        (Some((indent, ordered, _)), Some((top, node))) => indent == *top && ordered == node.ordered,
                        _ => false,
                    };
                    if cont {
                        i = next;
                        continue;
                    }
                    break;
                }
                let Some((indent, ordered, body)) = list_marker(current) else { break };
                while let Some((top, _)) = stack.last() {
                    if indent < *top {
                        let (_, node) = stack.pop().unwrap();
                        match stack.last_mut() {
                            Some((_, parent)) => {
                                if let Some(last) = parent.items.last_mut() {
                                    last.children.push(node);
                                } else {
                                    parent.items.push(ListItem { text: String::new(), children: vec![node] });
                                }
                            }
                            None => root = Some(node),
                        }
                    } else {
                        break;
                    }
                }
                let same = stack.last().map(|(top, _)| *top == indent).unwrap_or(false);
                if !same {
                    stack.push((indent, ListNode { ordered, items: Vec::new() }));
                }
                if let Some((_, node)) = stack.last_mut() {
                    node.items.push(ListItem { text: body.trim().to_string(), children: Vec::new() });
                }
                i += 1;
            }
            while let Some((_, node)) = stack.pop() {
                match stack.last_mut() {
                    Some((_, parent)) => {
                        if let Some(last) = parent.items.last_mut() {
                            last.children.push(node);
                        } else {
                            parent.items.push(ListItem { text: String::new(), children: vec![node] });
                        }
                    }
                    None => root = Some(node),
                }
            }
            if let Some(root) = root {
                render_list(&root, &mut out);
            }
            continue;
        }
        let mut body = String::new();
        while i < src.len() {
            let l = src[i];
            if l.trim().is_empty() || is_block_start(l) {
                break;
            }
            if !body.is_empty() {
                body.push('\u{a}');
            }
            body.push_str(l.trim_end());
            i += 1;
        }
        out.push_str(&format!("<p>{}</p>", inline(&body)));
    }
    out
}


fn user_item(text: &str) -> String {
    format!(r#"<div class="EvIC1a_flowItem"><div><div class="Sixlwa_userRow"><div data-slot="conversation.chat.user.before" style="display: contents;"></div><div class="Sixlwa_userStack"><div data-message-bubble="" data-text-interaction-source="" class="_bubble_1sgyj_1"><span class="BNl2La_root"><span data-slot="conversation.user.text.node" style="display: contents;"><span class="_plainRun_fbulu_6">{}</span></span></span></div></div></div></div></div>"#, esc(text))
}

fn think_card(summary: &str) -> String {
    format!(r##"<div class="lcKema_root" data-variant="think" data-state="ok" data-preview="true"><div class="_root_l4nv8_9"><div class="_header_l4nv8_16" data-disclosure-header="true"><div class="_row_l4nv8_26 lcKema_row" data-disclosure-row="true" data-expandable="true" role="button" tabindex="0" aria-expanded="false"><span class="_leading_l4nv8_52 lcKema_leading"><span class="_iconIdle_l4nv8_80"><svg width="14" height="14" data-icon="dsw-icon-think-outline-regular" aria-hidden="true"><use href="#dsw-icon-think-outline-regular"></use></svg></span><svg width="14" height="14" class="_chevronHover_l4nv8_86" data-icon="dsw-icon-chevron-down-outline-regular" aria-hidden="true"><use href="#dsw-icon-chevron-down-outline-regular"></use></svg></span><span class="_root_1rdzk_1"><span class="_content_1rdzk_11"><span class="_text_1rdzk_17 _title_l4nv8_104 lcKema_title">Think</span><span class="lcKema_separator" data-shimmer-decoration="true" aria-hidden="true"></span><span class="lcKema_summary"><span class="lcKema_summaryText"><span class="_text_1rdzk_17">{}</span></span></span></span></span></div></div></div></div>"##, esc(summary))
}

fn assistant_item(parts: &[Value]) -> String {
    let mut inner = String::new();
    for part in parts {
        let kind = part.get("type").and_then(|t| t.as_str()).unwrap_or("text");
        let text = part.get("text").and_then(|t| t.as_str()).unwrap_or("");
        match kind {
            "reasoning" => {
                if !text.trim().is_empty() { inner.push_str(&think_card(&summary_line(text))); }
            }
            "tool-call" => {}
            _ => {
                if !text.trim().is_empty() {
                    inner.push_str(r#"<div class="_markdown_rve47_5" data-text-interaction-source="">"#);
                    inner.push_str(&render_markdown(text));
                    inner.push_str("</div>");
                }
            }
        }
    }
    if inner.is_empty() { return String::new(); }
    format!(r#"<div class="EvIC1a_flowItem"><div><div class="hWmORq_root"><div class="hWmORq_body"><div>{}</div></div></div></div></div>"#, inner)
}

/// Pull image parts out of a tool result into renderable sources.
fn tool_images(data: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let content = data["message"]["content"].as_array().cloned().unwrap_or_default();
    for part in &content {
        let kind = part["type"].as_str().unwrap_or("");
        if kind != "image" {
            continue;
        }
        if let Some(url) = part["url"].as_str().or_else(|| part["source"]["url"].as_str()) {
            out.push(asset_url(url));
            continue;
        }
        let media = part["source"]["mediaType"]
            .as_str()
            .or_else(|| part["mediaType"].as_str())
            .or_else(|| part["mimeType"].as_str())
            .or_else(|| part["source"]["mimeType"].as_str())
            .unwrap_or("image/png");
        if let Some(data) = part["source"]["data"].as_str().or_else(|| part["data"].as_str()) {
            if data.starts_with("data:") {
                out.push(data.to_string());
            } else {
                out.push(format!("data:{media};base64,{data}"));
            }
        }
    }
    out
}

fn tool_image_block(images: &[String]) -> String {
    if images.is_empty() {
        return String::new();
    }
    let mut s = String::from(r#"<div class="EvIC1a_flowItem"><div><div class="hWmORq_root"><div class="hWmORq_body"><div class="_markdown_rve47_5">"#);
    for src in images {
        s.push_str(&format!(r##"<img class="_image_rve47_300" src="{}" alt="tool image" loading="lazy">"##, src));
    }
    s.push_str("</div></div></div></div></div>");
    s
}

fn tool_item(name: &str, summary: &str, ok: bool) -> String {
    let state = if ok { "ok" } else { "error" };
    format!(r##"<div class="EvIC1a_flowItem"><div><div class="o3BgMG_root" data-variant="code" data-tool="{}" data-state="{}"><div class="_root_l4nv8_9"><div class="_header_l4nv8_16" data-disclosure-header="true"><div class="_row_l4nv8_26 o3BgMG_row" data-disclosure-row="true" data-expandable="false"><span class="_leading_l4nv8_52 o3BgMG_leading"><span class="_iconIdle_l4nv8_80"><svg width="14" height="14" data-icon="dsw-icon-code-outline-regular" aria-hidden="true"><use href="#dsw-icon-code-outline-regular"></use></svg></span></span><span class="_root_1rdzk_1"><span class="_content_1rdzk_11"><span class="_text_1rdzk_17 _title_l4nv8_104 o3BgMG_title">{}</span><span class="o3BgMG_sep">·</span><span class="o3BgMG_summary"><span class="_text_1rdzk_17">{}</span></span></span></span></div></div></div></div></div></div>"##, esc(name), state, esc(name), esc(summary))
}

fn load_sprite() -> std::collections::HashMap<String, (String, String, String)> {
    std::fs::read_to_string(asset("sprite.json")).ok()
        .and_then(|j| serde_json::from_str::<Vec<Value>>(&j).ok())
        .map(|v| v.into_iter().filter_map(|x| {
            let id = x["id"].as_str()?.to_string();
            let vb = x["vb"].as_str().unwrap_or("0 0 16 16").to_string();
            let inner = x["inner"].as_str()?.to_string();
            let attrs = x["attrs"].as_str().unwrap_or("").to_string();
            Some((id, (vb, inner, attrs)))
        }).collect())
        .unwrap_or_default()
}

fn expand_icons(page: &str, sprite: &std::collections::HashMap<String, (String, String, String)>) -> String {
    let mut out = String::with_capacity(page.len() + 65536);
    let mut i = 0usize;
    while let Some(rel) = page[i..].find("<use") {
        let pos = i + rel;
        let Some(tag_end) = page[pos..].find('>').map(|k| pos + k + 1) else { break };
        let tag = &page[pos..tag_end];
        let mut gt = tag_end;
        if page[tag_end..].starts_with("</use>") { gt = tag_end + 6; }
        let Some(hash) = tag.find("href=\u{22}#") else { out.push_str(&page[i..gt]); i = gt; continue };
        let id_start = pos + hash + 7;
        let Some(id_end_rel) = page[id_start..].find('"') else { break };
        let id = &page[id_start..id_start + id_end_rel];
        let Some((vb, inner, attrs)) = sprite.get(id) else { out.push_str(&page[i..gt]); i = gt; continue };
        match page[i..pos].rfind("<svg") {
            Some(svg_rel) => {
                let svg_pos = i + svg_rel;
                let svg_tag_end = page[svg_pos..].find('>').map(|k| svg_pos + k).unwrap_or(pos);
                let svg_tag = &page[svg_pos..svg_tag_end];
                if svg_tag.contains("viewBox") {
                    out.push_str(&page[i..svg_tag_end + 1]);
                } else {
                    out.push_str(&page[i..svg_tag_end]);
                    out.push_str(&format!(" viewBox=\u{22}{vb}\u{22}>"));
                }
                out.push_str(&format!("<g {attrs}>{inner}</g>"));
            }
            None => {
                out.push_str(&page[i..pos]);
                out.push_str(&format!("<g {attrs}>{inner}</g>"));
            }
        }
        i = gt;
    }
    out.push_str(&page[i..]);
    out
}

// ---------------------------------------------------------------- permission / model chips

const GLYPH_READ_ONLY: &str = r##"<path d="M5.08545 8.13775L7.18455 10.2368C7.26636 10.3187 7.4003 10.3142 7.47649 10.2271L11.5148 5.61194" stroke="currentColor"></path><path d="M6.59624 2.14853C7.50155 1.80917 8.49914 1.80919 9.40444 2.14859L13.9245 3.84317V7.11961C13.9245 11.6089 10.5565 13.5975 8.00035 14.5779C5.44423 13.5975 2.07544 11.6089 2.07544 7.11961V3.84317L6.59624 2.14853Z" stroke="currentColor" stroke-linejoin="round"></path>"##;
const GLYPH_WORKSPACE_WRITE: &str = r##"<path d="M6.4209 1.68067C7.43922 1.299 8.56177 1.29898 9.58008 1.68067L14.0996 3.375C14.2946 3.44811 14.4236 3.63455 14.4238 3.84278V6.89063C14.1115 6.71853 13.7761 6.58312 13.4238 6.48926V4.18946L9.22852 2.61621C8.43657 2.31947 7.56341 2.31939 6.77148 2.61621L2.5752 4.18946V7.11914C2.5752 11.1796 5.52369 13.056 8 14.0391C8.27653 13.9293 8.55827 13.8067 8.8418 13.6729C9.07101 13.9468 9.33228 14.1929 9.62012 14.4053C9.12409 14.6579 8.63578 14.8696 8.17871 15.0449C8.0637 15.0889 7.93628 15.0889 7.82129 15.0449C5.22011 14.0472 1.5752 11.9381 1.5752 7.11914V3.84278C1.57541 3.63469 1.70463 3.44821 1.89941 3.375L6.4209 1.68067Z" fill="currentColor"></path><path d="M5.26392 6.60339H10.7361" stroke="currentColor"></path><path d="M5.26392 9.86902H8.32833" stroke="currentColor"></path><path d="M10.0317 13.2229C10.263 13.3929 10.4943 13.563 10.7256 13.733C10.7932 13.6482 10.8608 13.5634 10.9284 13.4786C12.1455 11.9522 13.3626 10.4258 14.5798 8.89935C14.6474 8.81455 14.715 8.72975 14.7826 8.64495C14.4143 8.37419 14.046 8.10344 13.6777 7.83268C13.6169 7.92252 13.5562 8.01236 13.4954 8.10219C12.4016 9.71926 11.3078 11.3363 10.214 12.9534C10.1532 13.0432 10.0924 13.1331 10.0317 13.2229Z" fill="currentColor"></path><path d="M12.6516 12.6696C12.6516 12.925 12.6516 13.1804 12.6516 13.4359C12.6952 13.4378 12.7387 13.4398 12.7823 13.4417C13.5663 13.4768 14.3504 13.5118 15.1345 13.5469C15.178 13.5488 15.2216 13.5508 15.2651 13.5527C15.2651 13.2194 15.2651 12.8861 15.2651 12.5527C15.2216 12.5547 15.178 12.5566 15.1345 12.5586C14.3504 12.5936 13.5663 12.6287 12.7823 12.6637C12.7387 12.6657 12.6952 12.6676 12.6516 12.6696Z" fill="currentColor"></path>"##;
const GLYPH_FULL_ACCESS: &str = r##"<path d="M6.59624 2.14853C7.50155 1.80917 8.49914 1.80919 9.40444 2.14859L13.9245 3.84317V7.11961C13.9245 11.6089 10.5565 13.5975 8.00035 14.5779C5.44423 13.5975 2.07544 11.6089 2.07544 7.11961V3.84317L6.59624 2.14853Z" stroke="currentColor" stroke-linejoin="round"></path><path d="M8 4.39209V9.89209" stroke="currentColor"></path><path d="M8 10.8081V11.8081" stroke="currentColor"></path>"##;

const CHEVRON_DOWN: &str = r##"<svg width="14" height="14" data-icon="dsw-icon-chevron-down-outline-regular" aria-hidden="true"><use href="#dsw-icon-chevron-down-outline-regular"></use></svg>"##;
const CHEVRON_RIGHT: &str = r##"<svg width="14" height="14" class="_7KE1Ra_cellChevron" data-icon="dsw-icon-chevron-right-outline-regular" aria-hidden="true"><use href="#dsw-icon-chevron-right-outline-regular"></use></svg>"##;

#[derive(Clone, Debug, PartialEq, Default)]
struct Sel { provider: String, model: String, effort: Option<String> }

#[derive(Clone)]
struct ModelEntry { provider: String, id: String, name: String, efforts: Vec<(String, String)> }

#[derive(Clone, Copy, PartialEq, Debug)]
enum Menu { None, Permission, Model, ModelList, Effort }

impl Default for Menu { fn default() -> Self { Menu::None } }

#[derive(Clone, Default)]
struct UiState {
    perm_options: Vec<(String, String)>,
    perm_current: String,
    groups: Vec<(String, String, Vec<ModelEntry>)>,
    sel: Option<Sel>,
    menu: Menu,
}

fn perm_label(value: &str) -> String {
    match value {
        "read-only" => "Read Only".to_string(),
        "workspace-write" => "Workspace Write".to_string(),
        "danger-full-access" => "Full access".to_string(),
        other => other.to_string(),
    }
}

fn perm_glyph(value: &str) -> &'static str {
    match value {
        "read-only" => GLYPH_READ_ONLY,
        "workspace-write" => GLYPH_WORKSPACE_WRITE,
        _ => GLYPH_FULL_ACCESS,
    }
}

fn shield_svg(inner: &str) -> String {
    format!(r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" xmlns="http://www.w3.org/2000/svg" aria-hidden="true" stroke-width="1">{inner}</svg>"##)
}

impl UiState {
    fn model_name(&self) -> String {
        let Some(sel) = self.sel.as_ref() else { return "Select model".to_string() };
        for (_, _, models) in &self.groups {
            for m in models {
                if m.provider == sel.provider && m.id == sel.model { return m.name.clone(); }
            }
        }
        sel.model.clone()
    }

    fn current_model(&self) -> Option<&ModelEntry> {
        let sel = self.sel.as_ref()?;
        self.groups.iter().flat_map(|(_, _, ms)| ms.iter())
            .find(|m| m.provider == sel.provider && m.id == sel.model)
    }

    fn effort_label(&self) -> String {
        let Some(sel) = self.sel.as_ref() else { return "Default".to_string() };
        let Some(id) = sel.effort.as_ref() else { return "Default".to_string() };
        if let Some(m) = self.current_model() {
            for (eid, ename) in &m.efforts { if eid == id { return ename.clone(); } }
        }
        id.clone()
    }

    fn perm_trigger(&self) -> String {
        let label = perm_label(&self.perm_current);
        let chevron_cls = if self.menu == Menu::Permission { "iWlSmW_chevron iWlSmW_chevronOpen" } else { "iWlSmW_chevron" };
        let expanded = self.menu == Menu::Permission;
        format!(r##"<span class="_root_1p5el_1"><button type="button" id="perm-trigger" class="iWlSmW_trigger" aria-label="Access mode, current: {label}" aria-haspopup="menu" aria-expanded="{expanded}"><span class="iWlSmW_triggerIcon" aria-hidden="true">{icon}</span><span class="iWlSmW_triggerLabel">{label}</span><span id="perm-chevron" class="{chevron_cls}" aria-hidden="true">{ch}</span></button></span>"##,
            icon = shield_svg(perm_glyph(&self.perm_current)), ch = CHEVRON_DOWN)
    }

    fn model_trigger(&self) -> String {
        let name = self.model_name();
        let effort = self.effort_label();
        let open = matches!(self.menu, Menu::Model | Menu::ModelList | Menu::Effort);
        let chevron_cls = if open { "_7KE1Ra_chevron _7KE1Ra_chevronOpen" } else { "_7KE1Ra_chevron" };
        format!(r##"<div class="_7KE1Ra_root"><button type="button" id="model-trigger" class="_7KE1Ra_trigger" aria-label="Select model, current {name}, reasoning effort {effort}" aria-haspopup="menu" aria-expanded="{open}" title="{name} · {effort}"><svg width="16" height="16" class="_7KE1Ra_triggerIcon" data-icon="dsw-icon-data-outline-regular" aria-hidden="true"><use href="#dsw-icon-data-outline-regular"></use></svg><span class="_7KE1Ra_triggerLabel">{name}</span><span class="_7KE1Ra_triggerEffort">{effort}</span><svg id="model-chevron" width="14" height="14" class="{chevron_cls}" data-icon="dsw-icon-chevron-down-outline-regular" aria-hidden="true"><use href="#dsw-icon-chevron-down-outline-regular"></use></svg></button></div>"##)
    }

    /// All four popups are pre-rendered once per page build; opening one only
    /// flips their display, so no document rebuild happens on a chip click.
    fn menus_html(&self) -> String {
        let mut out = String::with_capacity(8192);
        out.push_str(r##"<div id="menu-layer" style="position:absolute;bottom:calc(100% + 6px);left:0;right:0;z-index:1200;display:flex;padding:0 16px">"##);
        out.push_str(&self.permission_menu());
        out.push_str(&self.model_root_menu());
        out.push_str(&self.model_list_menu());
        out.push_str(&self.effort_menu());
        out.push_str("</div>");
        let open_id = match self.menu {
            Menu::Permission => "menu-permission",
            Menu::Model => "menu-model",
            Menu::ModelList => "menu-model-list",
            Menu::Effort => "menu-effort",
            Menu::None => "",
        };
        if !open_id.is_empty() {
            out = out.replacen(
                &format!(r#"id="{open_id}" style="display:none;"#),
                &format!(r##"id="{open_id}" style="display:flex;"##),
                1,
            );
        }
        out
    }

    fn permission_menu(&self) -> String {
        let mut items = String::new();
        for (i, (value, _name)) in self.perm_options.iter().enumerate() {
            let label = perm_label(value);
            let current = *value == self.perm_current;
            let extra = if current { " _selected_1p5el_240" } else { "" };
            let check = "";
            let style = if current { r#" style="background:#eef0f3""# } else { "" };
            let label_style = if current { r#" style="font-weight:600""# } else { "" };
            items.push_str(&format!(r##"<div class="_itemWrap_1p5el_105"><button type="button" role="menuitem" id="perm-opt-{i}" class="_item_1p5el_105{extra}"{style}><span aria-hidden="true" class="_itemIcon_1p5el_165">{icon}</span><span class="_itemLabel_1p5el_208"{label_style}>{label}</span>{check}</button></div>"##,
                icon = shield_svg(perm_glyph(value))));
        }
        let body = format!(r##"<div role="menu" data-menu-material="translucent" class="_surface_1w4el_1 _list_1p5el_7 _scrollable_1p5el_19" style="position:relative;top:auto;left:auto;width:100%;box-sizing:border-box;border-radius:0;overflow:visible;box-shadow:none;background:transparent;--dsh-anchored-max-width:420px;--dsh-anchored-max-height:320px"><div class="_viewport_1p5el_19" role="presentation">{items}</div></div>"##);
        menu_panel("menu-permission", "margin-right:auto;width:262px", &body)
    }

    fn model_root_menu(&self) -> String {
        let name = self.model_name();
        let effort = self.effort_label();
        let body = format!(r##"<div role="menu" data-menu-material="translucent" class="_surface_1w4el_1 _7KE1Ra_menu" style="position:relative;top:auto;left:auto;width:100%;box-sizing:border-box;border-radius:0;overflow:visible;box-shadow:none;background:transparent;--dsh-anchored-max-width:420px;--dsh-anchored-max-height:320px"><button type="button" role="menuitem" id="model-cell-model" class="_7KE1Ra_cell"><span class="_7KE1Ra_cellLabel">Model</span><span class="_7KE1Ra_cellValue">{name}</span>{chevr}</button><button type="button" role="menuitem" id="model-cell-effort" class="_7KE1Ra_cell"><span class="_7KE1Ra_cellLabel">Effort</span><span class="_7KE1Ra_cellValue">{effort}</span>{chevr}</button></div>"##, chevr = CHEVRON_RIGHT);
        menu_panel("menu-model", "margin-left:auto;margin-right:46px;width:300px", &body)
    }

    fn model_list_menu(&self) -> String {
        let mut groups = String::new();
        for (gi, (_pid, pname, models)) in self.groups.iter().enumerate() {
            groups.push_str(&format!(r##"<section role="group" class="_group_121p8_1"><span aria-hidden="true" data-menu-group-start="" class="_start_121p8_5"></span><div data-menu-group-heading="" class="_heading_121p8_19">{pname}</div>"##));
            for (mi, m) in models.iter().enumerate() {
                let current = self.sel.as_ref().map(|s| s.provider == m.provider && s.model == m.id).unwrap_or(false);
                let extra = if current { " _7KE1Ra_selected _7KE1Ra_optionActive" } else { "" };
                let check = "";
                let label_style = if current { r#" style="font-weight:600""# } else { "" };
                let name = esc(&m.name);
                let style = if current { r#" style="background:#eef0f3""# } else { "" };
                groups.push_str(&format!(r##"<button type="button" role="menuitemradio" aria-checked="{current}" id="model-opt-{gi}-{mi}" class="_7KE1Ra_option _7KE1Ra_modelOption{extra}"{style} title="{name}"><span class="_7KE1Ra_optionCopy"><span class="_7KE1Ra_modelName"{label_style}>{name}</span></span><span class="_7KE1Ra_check">{check}</span></button>"##));
            }
            groups.push_str("</section>");
        }
        let body = format!(r##"<div role="menu" data-menu-material="translucent" class="_surface_1w4el_1 _7KE1Ra_menu" style="position:relative;top:auto;left:auto;width:100%;box-sizing:border-box;border-radius:0;overflow:visible;box-shadow:none;background:transparent;--dsh-anchored-max-width:420px;--dsh-anchored-max-height:320px"><div class="_7KE1Ra_groups scrollable" id="model-list" style="max-height:320px;overflow-y:hidden;min-width:320px">{groups}</div></div>"##);
        menu_panel("menu-model-list", "margin-left:auto;margin-right:46px;width:352px", &body)
    }

    fn effort_menu(&self) -> String {
        let mut items = String::new();
        let mut push = |i: usize, label: &str, current: bool| {
            let extra = if current { " _7KE1Ra_selected" } else { "" };
            let style = if current { r#" style="background:#eef0f3""# } else { "" };
            let label_style = if current { r#" style="font-weight:600""# } else { "" };
            items.push_str(&format!(r##"<button type="button" role="menuitemradio" aria-checked="{current}" id="effort-opt-{i}" class="_7KE1Ra_option{extra}"{style}><span class="_7KE1Ra_optionCopy"><span class="_7KE1Ra_modelName"{label_style}>{label}</span></span></button>"##));
        };
        let cur = self.sel.as_ref().and_then(|s| s.effort.clone());
        push(0, "Default", cur.is_none());
        if let Some(m) = self.current_model() {
            for (i, (_eid, ename)) in m.efforts.iter().enumerate() {
                let is_cur = cur.as_deref() == Some(_eid.as_str());
                push(i + 1, &esc(ename), is_cur);
            }
        }
        let body = format!(r##"<div role="menu" data-menu-material="translucent" class="_surface_1w4el_1 _7KE1Ra_menu" style="position:relative;top:auto;left:auto;width:100%;box-sizing:border-box;border-radius:0;overflow:visible;box-shadow:none;background:transparent;--dsh-anchored-max-width:420px;--dsh-anchored-max-height:320px"><div class="_7KE1Ra_groups scrollable" id="effort-list">{items}</div></div>"##);
        menu_panel("menu-effort", "margin-left:auto;margin-right:46px;width:232px", &body)
    }
}

fn menu_panel(id: &str, align: &str, inner: &str) -> String {
    format!(r##"<div id="{id}" style="display:none;flex:0 0 auto;flex-direction:column;{align};background:#fcfdfe;border:0.5px solid #e6e7ea;border-radius:12px;box-shadow:0 4px 18px rgba(0,0,0,0.12);overflow:hidden">{inner}</div>"##)
}

struct PageSource { bare: String }

fn page_source() -> &'static PageSource {
    use std::sync::OnceLock;
    static SRC: OnceLock<PageSource> = OnceLock::new();
    SRC.get_or_init(|| {
        let shell = std::fs::read_to_string(asset("shell.html")).expect("shell");
        let mut bare = String::with_capacity(shell.len());
        let mut css = String::new();
        let mut rest = shell.as_str();
        while let Some(start) = rest.find("<style") {
            let Some(tag_end) = rest[start..].find('>').map(|k| start + k + 1) else { break };
            bare.push_str(&rest[..start]);
            let Some(end) = rest[tag_end..].find("</style>").map(|k| tag_end + k) else { break };
            css.push_str(&rest[tag_end..end]);
            css.push('\u{a}');
            rest = &rest[end + 8..];
        }
        bare.push_str(rest);
        let styled = format!("<style id=\u{22}dsh-blitz-all\u{22}>{css}\u{a}{LAYOUT_FIX_CSS}</style>");
        let bare = bare.replacen("</head>", &format!("{styled}</head>"), 1);
        let _ = &css;
        PageSource { bare }
    })
}




// ---------------------------------------------------------------- stats pills

/// The composer dock readings, in the exact shapes DSH renders them.
#[derive(Clone, Default, PartialEq)]
struct Stats {
    activity: String,
    speed: String,
    usage: String,
    cache: String,
    context: String,
    ring: String,
}

fn fmt_compact(value: f64) -> String {
    let scaled = |c: f64| {
        if c >= 100.0 {
            format!("{}", c.round() as i64)
        } else {
            format!("{}", (c * 10.0).round() / 10.0)
        }
    };
    if value < 1_000.0 {
        format!("{}", value as i64)
    } else if value < 1_000_000.0 {
        format!("{}K", scaled(value / 1_000.0))
    } else {
        format!("{}M", scaled(value / 1_000_000.0))
    }
}

fn fmt_tps(value: f64) -> String {
    let c = value.max(0.0);
    if c >= 10.0 {
        format!("{}", c.round() as i64)
    } else {
        format!("{}", (c * 10.0).round() / 10.0)
    }
}

fn stats_from(values: &Value) -> Stats {
    let mut s = Stats::default();
    let st = &values["sessionStats"];
    let turns = st["turns"].as_u64().unwrap_or(0);
    let steps = st["steps"].as_u64().unwrap_or(0);
    let decode_ms = st["decodeMs"].as_f64().unwrap_or(0.0);
    let decode_tokens = st["decodeTokens"].as_f64().unwrap_or(0.0);
    if steps > 0 {
        s.activity = format!("{turns} turns {steps} steps");
    }
    if decode_ms > 0.0 {
        s.speed = format!("{} tok/s", fmt_tps(decode_tokens / (decode_ms / 1_000.0)));
    }
    let usage = &values["tokenUsage"];
    let uncached = usage["uncachedInputTokens"].as_f64().unwrap_or(0.0);
    let output = usage["outputTokens"].as_f64().unwrap_or(0.0);
    let read = usage["cacheReadTokens"].as_f64().unwrap_or(0.0);
    let write = usage["cacheWriteTokens"].as_f64().unwrap_or(0.0);
    let billed = uncached + read + write;
    if billed > 0.0 || output > 0.0 {
        s.usage = format!("{} tok", fmt_compact(billed + output));
    }
    if billed > 0.0 {
        let exact = read / billed * 100.0;
        let rounded = exact.round();
        let text = if rounded >= 100.0 && exact < 100.0 {
            format!("{exact:.1}")
        } else {
            format!("{}", rounded as i64)
        };
        s.cache = format!("Cache hit {text}%");
    }
    let cp = &values["contextPressure"];
    let used = cp["projectedTokens"].as_f64().or_else(|| cp["pressureTokens"].as_f64());
    if let (Some(used), Some(window)) = (used, cp["contextWindow"].as_f64()) {
        if window > 0.0 {
            let pct = ((used / window) * 100.0).round().min(100.0);
            s.context = format!("{}%", pct as i64);
            let circumference = 2.0 * std::f64::consts::PI * 5.5;
            s.ring = format!("{:.6} {:.6}", circumference * pct / 100.0, circumference);
        }
    }
    s
}

async fn fetch_stats(cfg: &Cfg, session: &str) -> Option<Stats> {
    let mut probe = cfg.clone();
    probe.session = session.to_string();
    let v = rpc(&probe, "session/projections", serde_json::json!({ "request": { "sessionId": session } })).await.ok()?;
    Some(stats_from(&v["values"]))
}

// ---------------------------------------------------------------- new task hero

#[derive(Clone, Default)]
struct Preset {
    id: String,
    name: String,
    #[allow(dead_code)]
    description: String,
}

#[derive(Clone, Copy, PartialEq)]
enum HeroMenu {
    None,
    Workspace,
    Preset,
}

impl Default for HeroMenu {
    fn default() -> Self {
        HeroMenu::None
    }
}

/// The empty "new task" screen: no Session yet, so the workspace chip and the
/// agent-preset seat choose what the first prompt will create.
#[derive(Clone, Default)]
struct Hero {
    active: bool,
    /// Blank Session already created for the chosen workspace.
    session: Option<String>,
    workspace: Option<String>,
    preset: Option<String>,
    presets: Vec<Preset>,
    menu: HeroMenu,
}

fn load_presets_now(cfg: &Cfg) -> Vec<Preset> {
    let rt = match rt_current() {
        Ok(rt) => rt,
        Err(_) => return Vec::new(),
    };
    rt.block_on(load_presets(cfg))
}

fn preset_name(id: &str) -> String {
    match id {
        "standard" => "Standard mode",
        "ptc" => "PTC mode",
        "minimal" => "Minimal mode",
        "cordis" => "Creator mode",
        other => other,
    }
    .to_string()
}

async fn load_presets(cfg: &Cfg) -> Vec<Preset> {
    let mut out = Vec::new();
    if let Ok(v) = rpc(cfg, "agentPresets/list", serde_json::json!({})).await {
        let mut list: Vec<(i64, String)> = Vec::new();
        for p in v["presets"].as_array().cloned().unwrap_or_default() {
            let id = p["id"].as_str().unwrap_or("").to_string();
            if id.is_empty() {
                continue;
            }
            list.push((p["order"].as_i64().unwrap_or(0), id));
        }
        list.sort_by_key(|(order, _)| *order);
        for (_, id) in list {
            out.push(Preset { name: preset_name(&id), id, description: String::new() });
        }
    }
    out
}

impl Hero {
    fn html(&self, workspaces: &[WsRow]) -> String {
        if !self.active {
            return String::new();
        }
        let workspace_label = self
            .workspace
            .as_ref()
            .and_then(|id| workspaces.iter().find(|w| &w.id == id))
            .map(|w| w.title.clone());
        let label = workspace_label.clone().unwrap_or_else(|| "Choose workspace".to_string());
        let preset = self.preset.clone().unwrap_or_default();
        let preset_label = if preset.is_empty() { String::new() } else { preset_name(&preset) };
        let fish = BRAND_MARK
            .replacen("width=\"24\"", "width=\"46\"", 1)
            .replacen("height=\"17.6580310880829\"", "height=\"34\"", 1);
        let mut s = String::new();
        s.push_str(r##"<div class="wSkVaW_composerHero" id="hero-stack" style="display:flex;flex-direction:column;position:relative;z-index:20;width:min(712px,100%);align-self:center;padding-bottom:32px;gap:8px">"##);
        s.push_str(r##"<div class="pXSMma_root"><div class="pXSMma_stack"><div class="pXSMma_headline"><span class="pXSMma_fishHitbox">"##);
        s.push_str(&fish);
        s.push_str(r##"</span><span class="pXSMma_titleGroup"><span>Into the Unknown</span><span class="pXSMma_previewBadge">Preview</span></span></div><div class="pXSMma_body"></div></div></div>"##);
        s.push_str(r##"<div class="wSkVaW_heroWorkspaceRow">"##);
        s.push_str(r##"<span style="position:relative;display:inline-flex">"##);
        s.push_str(r##"<button type="button" id="hero-ws" class="pXSMma_workspace" aria-label="Choose workspace" aria-haspopup="menu" aria-expanded=""##);
        s.push_str(if self.menu == HeroMenu::Workspace { "true" } else { "false" });
        s.push_str(r##"">"##);
        s.push_str(&icon(if workspace_label.is_some() { "dsw-icon-folder-open-regular" } else { "dsw-icon-folder-close-regular" }, 16));
        s.push_str(&format!(r##"<span class="pXSMma_workspaceLabel">{}</span>"##, esc(&label)));
        s.push_str(&icon("dsw-icon-chevron-down-outline-regular", 12));
        s.push_str(r##"</button>"##);
        s.push_str(&self.menu_html(HeroMenu::Workspace, workspaces));
        s.push_str(r##"</span>"##);
        if !preset_label.is_empty() {
            s.push_str(r##"<span class="_root_1p5el_1 cubgiG_menuAnchor" style="position:relative;display:inline-flex">"##);
            s.push_str(r##"<button type="button" id="hero-preset" class="cubgiG_seat" aria-label="Agent preset" aria-haspopup="menu" aria-expanded=""##);
            s.push_str(if self.menu == HeroMenu::Preset { "true" } else { "false" });
            s.push_str(r##"" title="Choose the agent preset for your new task">"##);
            s.push_str(&icon_classed("dsw-icon-agent-preset-outline-regular", 14, "cubgiG_seatIcon"));
            s.push_str(&format!(r##"<span class="cubgiG_seatLabel">{}</span>"##, esc(&preset_label)));
            s.push_str(&icon_classed("dsw-icon-chevron-down-outline-regular", 12, "cubgiG_chevron"));
            s.push_str(r##"</button>"##);
            s.push_str(&self.menu_html(HeroMenu::Preset, workspaces));
            s.push_str(r##"</span>"##);
        }
        s.push_str(r##"</div></div>"##);
        s
    }

    /// One popup, anchored above its chip. Heights are computed exactly rather
    /// than by percentage so the box lands without the bottom property.
    fn menu_html(&self, kind: HeroMenu, workspaces: &[WsRow]) -> String {
        let (id, prefix, entries, current): (&str, &str, Vec<(String, String)>, Option<&str>) = match kind {
            HeroMenu::Workspace => (
                "hero-menu-ws",
                "hero-ws-opt-",
                workspaces.iter().map(|w| (w.id.clone(), w.title.clone())).collect(),
                self.workspace.as_deref(),
            ),
            HeroMenu::Preset => (
                "hero-menu-preset",
                "hero-preset-opt-",
                self.presets.iter().map(|p| (p.id.clone(), p.name.clone())).collect(),
                self.preset.as_deref(),
            ),
            HeroMenu::None => return String::new(),
        };
        if entries.is_empty() {
            return String::new();
        }
        let rows = entries.len() as i64;
        let height = rows * 34 + 8 + 2;
        let top = -(height + 44);
        let mut s = format!(
            r##"<div id="{id}" class="_list_1p5el_7" role="menu" style="position:absolute;top:{top}px;left:0;z-index:200;display:{};min-width:232px;max-height:320px;overflow-y:auto;padding:4px;background:#fcfdfe;border:0.5px solid #e0e2e6;border-radius:12px;box-shadow:0 10px 28px rgba(15,23,42,0.16)">"##,
            if self.menu == kind { "flex" } else { "none" }
        );
        for (i, (id, name)) in entries.iter().enumerate() {
            let active = current == Some(id.as_str());
            s.push_str(&format!(
                r##"<div class="_itemWrap_1p5el_105"><button type="button" id="{prefix}{i}" class="_item_1p5el_105{}" role="menuitem"><span class="_itemLabel_1p5el_208">{name}</span></button></div>"##,
                if active { " _selected_1p5el_240" } else { "" },
                prefix = prefix,
                name = esc(name),
            ));
        }
        s.push_str("</div>");
        s
    }
}

// ---------------------------------------------------------------- sidebar

const BRAND_MARK: &str = include_str!("../assets/brand_mark.svg");
const BRAND_NAME: &str = include_str!("../assets/brand_name.svg");
/// Sessions rendered per workspace before the "Show N more" row.
const SIDEBAR_PREVIEW: usize = 6;

fn icon(id: &str, size: u32) -> String {
    format!(r##"<svg width="{size}" height="{size}" data-icon="{id}" aria-hidden="true"><use href="#{id}"></use></svg>"##)
}

fn icon_classed(id: &str, size: u32, class: &str) -> String {
    format!(r##"<svg width="{size}" height="{size}" class="{class}" data-icon="{id}" aria-hidden="true"><use href="#{id}"></use></svg>"##)
}

#[derive(Clone, Default)]
struct WsRow {
    id: String,
    title: String,
    session_ids: Vec<String>,
}

#[derive(Clone, Default)]
struct SbSession {
    id: String,
    title: String,
    updated_at: i64,
    running: bool,
}

/// Everything the left pane shows: the Workspace order from workspace/follow
/// plus the Session summaries from session/list.
#[derive(Clone, Default)]
struct Sidebar {
    workspaces: Vec<WsRow>,
    archived: Vec<String>,
    sessions: Vec<SbSession>,
    current: String,
    expanded: bool,
    show_all: std::collections::HashSet<String>,
    collapsed: std::collections::HashSet<String>,
}

fn age_label(updated_at_ms: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let mins = ((now - updated_at_ms).max(0) / 60_000).max(1);
    if mins < 60 {
        format!("{mins}min")
    } else if mins < 1_440 {
        format!("{}h", mins / 60)
    } else {
        format!("{}d", mins / 1_440)
    }
}

impl Sidebar {
    /// Workspace groups in registration order, then the ungrouped remainder.
    fn groups(&self) -> Vec<(String, String, Vec<SbSession>)> {
        let archived: std::collections::HashSet<&str> = self.archived.iter().map(String::as_str).collect();
        let mut placed: std::collections::HashSet<&str> = Default::default();
        let mut out: Vec<(String, String, Vec<SbSession>)> = Vec::new();
        for ws in &self.workspaces {
            let mut rows = Vec::new();
            for sid in &ws.session_ids {
                if archived.contains(sid.as_str()) {
                    continue;
                }
                placed.insert(sid.as_str());
                if let Some(s) = self.sessions.iter().find(|s| &s.id == sid) {
                    rows.push(s.clone());
                }
            }
            out.push((ws.id.clone(), ws.title.clone(), rows));
        }
        let mut rest: Vec<SbSession> = self
            .sessions
            .iter()
            .filter(|s| !archived.contains(s.id.as_str()) && !placed.contains(s.id.as_str()))
            .cloned()
            .collect();
        rest.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        if !rest.is_empty() {
            out.push((String::new(), "Ungrouped".to_string(), rest));
        }
        out
    }

    /// Session title for the conversation header; the id when unknown.
    fn title_of(&self, id: &str) -> String {
        self.sessions
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.title.clone())
            .unwrap_or_else(|| id.to_string())
    }

    /// Workspace the Session is filed under, if any.
    fn workspace_of(&self, id: &str) -> Option<String> {
        self.workspaces
            .iter()
            .find(|w| w.session_ids.iter().any(|s| s == id))
            .map(|w| w.id.clone())
    }

    fn row_html(&self, s: &SbSession) -> String {
        let title = esc(&s.title);
        let age = age_label(s.updated_at);
        let is_current = s.id == self.current;
        let selected = if is_current { "true" } else { "false" };
        let row_class = if is_current { "YDXeBa_sessionRow YDXeBa_selected" } else { "YDXeBa_sessionRow" };
        let leading = if s.running {
            r##"<svg class="_spinner_1i3xo_37" data-state="ongoing" width="14" height="14" viewBox="0 0 24 24" aria-hidden="true"><g class="_spinnerMotion_1i3xo_42"><circle class="_spinnerTrack_1i3xo_47" cx="12" cy="12" r="9.5"></circle><circle class="_spinnerArc_1i3xo_48" cx="12" cy="12" r="9.5"></circle></g></svg><span class="YDXeBa_visuallyHidden">Running</span>"##.to_string()
        } else {
            r##"<span data-slot="sidebar.session.row.leading" style="display: contents;"></span>"##.to_string()
        };
        format!(
            r##"<span class="_root_bgejw_3"><div data-row-key="session:{id}" id="sb-s-{id}" class="{row_class}" role="treeitem" aria-selected="{selected}" tabindex="0"><span class="YDXeBa_slot">{leading}</span><span class="YDXeBa_title">{title}</span><span class="YDXeBa_rowResting"><span class="YDXeBa_time YDXeBa_timestamp">{age}</span><span data-slot="sidebar.session.row.trailing" style="display: contents;"></span></span><span class="YDXeBa_rowActions"><button type="button" class="YDXeBa_iconButton" aria-label="Session actions for {title}" data-context-menu-trigger="">{ellipsis}</button><span class="YDXeBa_rowQuickActions"><div data-slot="sidebar.workspaces.session.row.action" style="display: contents;"><button type="button" class="YDXeBa_iconButton" aria-label="Archive session">{archive}</button><button type="button" class="YDXeBa_iconButton" aria-label="Pin session">{pin}</button></div></span></span></div></span>"##,
            id = s.id,
            selected = selected,
            row_class = row_class,
            title = title,
            age = age,
            leading = leading,
            ellipsis = icon("dsw-icon-ellipsis-outline-regular", 16),
            archive = icon("dsw-icon-archive-outline-regular", 14),
            pin = icon("dsw-icon-pin-outline-regular", 14),
        )
    }

    fn group_html(&self, idx: usize, key: &str, title: &str, rows: &[SbSession]) -> String {
        let open = !self.collapsed.contains(key);
        let is_ws = !key.is_empty();
        let name = esc(title);
        let folder_cls = if open {
            "YDXeBa_slot YDXeBa_folder YDXeBa_folderActive"
        } else {
            "YDXeBa_slot YDXeBa_folder"
        };
        // Both glyphs are rendered; collapsing flips their display instead of
        // rebuilding the pane.
        let folder = format!(
            r##"<span id="sb-foldo-{idx}" style="display:{}">{open}</span><span id="sb-foldc-{idx}" style="display:{}">{close}</span>"##,
            if open { "contents" } else { "none" },
            if open { "none" } else { "contents" },
            open = icon("dsw-icon-folder-open-regular", 16),
            close = icon("dsw-icon-folder-close-regular", 16),
        );
        let arrow = icon_classed(
            "dsw-icon-triangle-right-fill-regular",
            14,
            if open { "YDXeBa_arrow YDXeBa_arrowOpen" } else { "YDXeBa_arrow" },
        );
        let actions = if is_ws {
            format!(
                r##"<span class="YDXeBa_rowActions"><button type="button" class="YDXeBa_iconButton" data-context-menu-trigger="true" aria-label="Workspace actions for {name}" aria-haspopup="menu" aria-expanded="false">{ellipsis}</button><button type="button" class="YDXeBa_iconButton" id="sb-newws-{idx}" aria-label="New session in {name}">{chat}</button></span>"##,
                name = name,
                ellipsis = icon("dsw-icon-ellipsis-outline-regular", 16),
                chat = icon("dsw-icon-new-chat-outline-regular", 14),
            )
        } else {
            format!(
                r##"<span class="YDXeBa_rowActions"><button type="button" class="YDXeBa_iconButton" id="sb-newws-{idx}" aria-label="New session in {name}">{chat}</button></span>"##,
                name = name,
                chat = icon("dsw-icon-new-chat-outline-regular", 14),
            )
        };
        let project = format!(
            r##"<div class="YDXeBa_projectRow" data-row-key="workspace:{key}" id="sb-ws-{idx}" role="treeitem" aria-expanded="{open}" tabindex="0"><span class="{folder_cls}">{folder}</span><span class="YDXeBa_slot YDXeBa_chevron">{arrow}</span><span class="YDXeBa_projectText"><span class="YDXeBa_title">{name}</span></span>{actions}</div>"##,
            key = key,
            open = open,
            folder_cls = folder_cls,
            folder = folder,
            arrow = arrow,
            name = name,
            actions = actions,
        );
        let mut out = format!(
            r##"<div class="bhn1Oq_groupSection" id="sb-sec-{idx}" style="--dsh-workspace-indent: 0px;">"##,
        );
        if is_ws {
            out.push_str(r##"<span class="_root_bgejw_3">"##);
            out.push_str(&project);
            out.push_str("</span>");
        } else {
            out.push_str(&project);
        }
        if open {
            let show_all = self.show_all.contains(key);
            let visible = if show_all { rows.len() } else { rows.len().min(SIDEBAR_PREVIEW) };
            for s in rows.iter().take(visible) {
                out.push_str(&self.row_html(s));
            }
            if !show_all && rows.len() > visible {
                out.push_str(&format!(
                    r##"<span class="_root_bgejw_3"><button type="button" class="bhn1Oq_sessionOverflowButton" id="sb-more-{idx}" data-row-key="overflow:{key}" aria-expanded="false">Show {n} more sessions</button></span>"##,
                    n = rows.len(),
                ));
            }
        }
        out.push_str("</div>");
        out
    }

    /// The expanded pane. The collapsed rail stays in the captured shell.
    fn html(&self) -> String {
        let mut s = String::new();
        s.push_str(r##"<div id="sidebar-full" class="hHd-Xa_root hHd-Xa_quietBars" style="width:280px;"##);
        if !self.expanded {
            s.push_str("display:none;");
        }
        s.push_str(r##""><div class="hHd-Xa_logoRow" data-window-drag="true"><button type="button" class="hHd-Xa_brand hHd-Xa_wide" id="sb-brand" aria-label="New session"><span class="hHd-Xa_brandIdentity" aria-hidden="true"><span class="hHd-Xa_brandMark"><div data-slot="sidebar.brand.mark" style="display: contents;">"##);
        s.push_str(BRAND_MARK);
        s.push_str(r##"</div></span><span class="hHd-Xa_brandName"><div data-slot="sidebar.brand.name" style="display: contents;">"##);
        s.push_str(BRAND_NAME);
        s.push_str(r##"</div></span></span></button><button id="sb-toggle" data-sidebar-toggle="true" type="button" class="hHd-Xa_iconButton hHd-Xa_toggle" aria-label="Collapse sidebar">"##);
        s.push_str(&icon("dsw-icon-panel-left-outline-regular", 16));
        s.push_str(r##"</button></div><button type="button" class="hHd-Xa_newSession" id="sb-new" aria-label="New session"><span class="hHd-Xa_newSessionLabelMask"><span class="hHd-Xa_newSessionContent">"##);
        s.push_str(&icon("dsw-icon-new-chat-outline-medium", 14));
        s.push_str(r##"<span class="hHd-Xa_newSessionLabel hHd-Xa_wide">New Session</span></span></span></button>"##);
        s.push_str(r##"<div class="hHd-Xa_regionArea"><div data-slot="sidebar.workspaces" style="display: contents;"><div class="bhn1Oq_root"><div class="bhn1Oq_sectionHeader"><span class="bhn1Oq_sectionLabel bhn1Oq_wide">Workspaces</span><div class="bhn1Oq_searchSlot"><div class="bhn1Oq_search"><button type="button" class="bhn1Oq_searchButton" aria-label="Search sessions">"##);
        s.push_str(&icon("dsw-icon-search-outline-regular", 14));
        s.push_str(r##"</button><input class="bhn1Oq_searchInput" type="text" placeholder="Search session names" maxlength="500" tabindex="-1" value=""></div></div><div class="bhn1Oq_headerActions"><span class="_root_1p5el_1"><button type="button" class="bhn1Oq_iconButton bhn1Oq_wide" aria-label="View options">"##);
        s.push_str(&icon("dsw-icon-sliders-two-outline-regular", 16));
        s.push_str(r##"</button></span><button type="button" class="bhn1Oq_iconButton" aria-label="Add workspace">"##);
        s.push_str(&icon("dsw-icon-project-add-outline-regular", 16));
        s.push_str(r##"</button></div><span class="_root_1p5el_1"></span><div data-slot="sidebar.workspaces.directoryFlow" style="display: contents;"></div></div><div class="bhn1Oq_listArea"><div class="bhn1Oq_treeBody bhn1Oq_wide"><div class="bhn1Oq_list" id="sb-scroll" role="tree" aria-label="Sessions">"##);
        for (idx, (key, title, rows)) in self.groups().iter().enumerate() {
            s.push_str(&self.group_html(idx, key, title, rows));
        }
        s.push_str(r##"</div><div class="_exits_1sy3u_1" aria-hidden="true"></div><span class="bhn1Oq_fade"></span></div></div></div></div></div>"##);
        s.push_str(r##"<div class="hHd-Xa_footArea"><div class="hHd-Xa_settingsArea"><div data-slot="sidebar.settings" style="display: contents;"><div class="VOzbGW_triggerRow"><div data-slot="settings.launcher" style="display: contents;"><div class="yHnPSG_root" data-inset="false"><span class="_root_1p5el_1 yHnPSG_anchor"><button type="button" class="yHnPSG_trigger" data-collapsed="false" data-signed-out="true" aria-label="Account menu" aria-haspopup="menu" aria-expanded="false">"##);
        s.push_str(&icon("dsw-icon-ellipsis-outline-medium", 14));
        s.push_str(r##"<span class="yHnPSG_label">More</span></button></span></div></div></div></div></div></div></div>"##);
        s
    }
}

/// Reads the Workspace baseline: one workspace/follow opening frame.
async fn load_workspaces(cookie: &str, origin: &str) -> Result<(Vec<WsRow>, Vec<String>)> {
    let mux = format!("{}/api/remote.mux", origin.replacen("http", "ws", 1));
    let mut req = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(mux.as_str())?;
    {
        use tokio_tungstenite::tungstenite::http::HeaderValue;
        let h = req.headers_mut();
        h.insert("Origin", HeaderValue::from_str(origin)?);
        h.insert("Cookie", HeaderValue::from_str(cookie)?);
    }
    let (ws, _) = tokio_tungstenite::connect_async(req).await?;
    let (mut tx, mut rx) = ws.split();
    tx.send(Message::Text(open_frame("w", "workspace/follow", serde_json::json!({})).into())).await?;
    while let Some(Ok(msg)) = rx.next().await {
        let Message::Text(t) = msg else { continue };
        let v: Value = serde_json::from_str(&t.to_string())?;
        if v["streamId"] != "w" {
            continue;
        }
        let inner = &v["value"]["value"];
        let base = if inner["items"].is_array() { inner } else { &v["value"] };
        let Some(items) = base["items"].as_array() else { continue };
        let mut workspaces = Vec::new();
        for it in items {
            workspaces.push(WsRow {
                id: it["workspaceId"].as_str().unwrap_or("").to_string(),
                title: it["title"].as_str().unwrap_or("").to_string(),
                session_ids: it["sessionIds"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
                    .unwrap_or_default(),
            });
        }
        let archived = base["archivedSessionIds"]
            .as_array()
            .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        return Ok((workspaces, archived));
    }
    Err(anyhow!("no workspace baseline"))
}

async fn load_sidebar(cfg: &Cfg) -> Sidebar {
    let mut sb = Sidebar { current: cfg.session.clone(), ..Default::default() };
    match load_workspaces(&cfg.cookie, &cfg.origin).await {
        Ok((workspaces, archived)) => {
            sb.workspaces = workspaces;
            sb.archived = archived;
        }
        Err(e) => eprintln!("workspace baseline: {e}"),
    }
    match rpc(cfg, "session/list", serde_json::json!({ "_request": {} })).await {
        Ok(v) => {
            for it in v["items"].as_array().cloned().unwrap_or_default() {
                let id = it["sessionId"].as_str().unwrap_or("").to_string();
                if id.is_empty() || it["blank"].as_bool() == Some(true) {
                    continue;
                }
                let title = it["projections"]["values"]["title"].as_str().unwrap_or("").to_string();
                sb.sessions.push(SbSession {
                    id,
                    // DSH shows Untitled for a session whose title projection is absent.
                    title: if title.is_empty() { "Untitled".to_string() } else { title },
                    updated_at: it["updatedAt"].as_i64().unwrap_or(0),
                    running: it["running"].as_bool().unwrap_or(false),
                });
            }
        }
        Err(e) => eprintln!("session list: {e}"),
    }
    sb
}


fn build_page(items: &str, title: &str, ui: &UiState, sb: &Sidebar, hero: &Hero) -> String {
    let src = page_source();
    let live = if hero.active { "" } else { r##"<div id="live-think-wrap"><div class="lcKema_root" data-variant="think" data-preview="true"><div class="_root_l4nv8_9"><div class="_header_l4nv8_16"><div class="_row_l4nv8_26 lcKema_row"><span class="_leading_l4nv8_52 lcKema_leading"><span class="_iconIdle_l4nv8_80"><svg width="14" height="14" data-icon="dsw-icon-think-outline-regular" aria-hidden="true"><use href="#dsw-icon-think-outline-regular"></use></svg></span></span><span class="_root_1rdzk_1"><span class="_content_1rdzk_11"><span class="_text_1rdzk_17 _title_l4nv8_104 lcKema_title">Think</span><span class="lcKema_separator"></span><span class="lcKema_summary"><span class="lcKema_summaryText"><span class="_text_1rdzk_17" id="live-think"></span></span></span></span></span></div></div></div></div></div><div class="EvIC1a_flowItem"><div><div class="hWmORq_root"><div class="hWmORq_body"><div id="live-tail"></div></div></div></div></div>"## };
    let items = format!("{items}{live}");
    let page = src.bare.replacen("<!--ITEMS-->", &items, 1);
    let page = page.replacen("Hyprlandでこの演出を再現", &esc(title), 1);
    let page = page.replacen(r#"id="composer-card""#, r#"id="composer-card" style="z-index:30""#, 1);
    let page = page.replacen("<!--PERM-TRIGGER-->", &ui.perm_trigger(), 1);
    let page = page.replacen("<!--MODEL-TRIGGER-->", &ui.model_trigger(), 1);
    let page = page.replacen("<!--MENU-->", &ui.menus_html(), 1);
    let page = page.replacen("<!--SIDEBAR-FULL-->", &sb.html(), 1);
    let page = page.replacen("<!--HERO-->", &hero.html(&sb.workspaces), 1);
    let page = if sb.expanded {
        let page = page.replacen(
            "grid-template-columns: 56px minmax(0px, 1fr) minmax(0px, 0px)",
            "grid-template-columns: 280px minmax(0px, 1fr) minmax(0px, 0px)",
            1,
        );
        // The collapsed rail lives in the captured shell; hide it explicitly or
        // every rebuild would stack it under the expanded pane.
        page.replacen(
            r#"<div id="sidebar-rail" class="hHd-Xa_root hHd-Xa_collapsed hHd-Xa_quietBars">"#,
            r#"<div id="sidebar-rail" class="hHd-Xa_root hHd-Xa_collapsed hHd-Xa_quietBars" style="display:none">"#,
            1,
        )
    } else {
        page
    };
    expand_icons(&page, &load_sprite())
}

#[derive(Clone)]
struct Cfg { cookie: String, origin: String, session: String }

async fn fetch_items(cfg: &Cfg) -> Result<(i64, String)> {
    let mux = format!("{}/api/remote.mux", cfg.origin.replacen("http", "ws", 1));
    let mut req = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(mux.as_str())?;
    {
        use tokio_tungstenite::tungstenite::http::HeaderValue;
        let h = req.headers_mut();
        h.insert("Origin", HeaderValue::from_str(&cfg.origin)?);
        h.insert("Cookie", HeaderValue::from_str(&cfg.cookie)?);
    }
    let (ws, _) = tokio_tungstenite::connect_async(req).await?;
    let (mut tx, mut rx) = ws.split();
    let args = serde_json::json!({ "request": { "address": { "kind": "session", "sessionId": cfg.session }, "activate": true } });
    tx.send(Message::Text(open_frame("f", "session/follow", args).into())).await?;
    let snapshot = loop {
        let Some(Ok(msg)) = rx.next().await else { return Err(anyhow!("closed")) };
        let Message::Text(t) = msg else { continue };
        let v: Value = serde_json::from_str(&t.to_string())?;
        if v["streamId"] == "f" && v["value"]["type"] == "snapshot" { break v["value"].clone(); }
    };
    let seq = snapshot["cursor"].as_i64().unwrap_or(0);
    let all = snapshot["records"].as_array().cloned().unwrap_or_default();
    let skip = all.len().saturating_sub(200);
    let records: Vec<Value> = all.into_iter().skip(skip).collect();
    let mut items = String::new();
    let mut pending: std::collections::HashMap<String, (String, String)> = Default::default();
    for rec in &records {
        let e = &rec["event"];
        match e["type"].as_str().unwrap_or("") {
            "user/message" => {
                let kind = e["data"]["source"]["kind"].as_str().unwrap_or("user");
                if kind == "user" {
                    items.push_str(&user_item(&text_of(&e["data"])));
                }
            }
            "assistant/message" => {
                let parts = e["data"]["message"]["content"].as_array().cloned().unwrap_or_default();
                items.push_str(&assistant_item(&parts));
            }
            "tool/call" => {
                let name = e["data"]["name"].as_str().unwrap_or("tool").to_string();
                let args_s = e["data"]["arguments"].as_str().unwrap_or("");
                let summary = summary_of(&name, args_s);
                let call_id = e["data"]["callId"].as_str().unwrap_or("").to_string();
                pending.insert(call_id, (name, summary));
            }
            "tool/result" => {
                let call_id = e["data"]["message"]["toolCallId"].as_str().unwrap_or("").to_string();
                let ok = e["data"].get("error").is_none();
                let (name, summary) = pending.remove(&call_id).unwrap_or(("tool".into(), String::new()));
                items.push_str(&tool_item(&name, &summary, ok));
                items.push_str(&tool_image_block(&tool_images(&e["data"])));
            }
            _ => {}
        }
    }
    Ok((seq, items))
}

async fn pick_session(cookie: &str, origin: &str) -> Result<String> {
    let mux = format!("{}/api/remote.mux", origin.replacen("http", "ws", 1));
    let mut req = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(mux.as_str())?;
    {
        use tokio_tungstenite::tungstenite::http::HeaderValue;
        let h = req.headers_mut();
        h.insert("Origin", HeaderValue::from_str(origin)?);
        h.insert("Cookie", HeaderValue::from_str(cookie)?);
    }
    let (ws, _) = tokio_tungstenite::connect_async(req).await?;
    let (mut tx, mut rx) = ws.split();
    tx.send(Message::Text(open_frame("w", "workspace/follow", serde_json::json!({})).into())).await?;
    while let Some(Ok(msg)) = rx.next().await {
        let Message::Text(t) = msg else { continue };
        let v: Value = serde_json::from_str(&t.to_string())?;
        if v["type"] == "item" && v["streamId"] == "w" {
            if let Some(id) = v["value"]["value"]["items"][0]["sessionIds"][0].as_str() {
                return Ok(id.to_string());
            }
            break;
        }
    }
    Err(anyhow!("no session"))
}

async fn post_prompt(cfg: &Cfg, text: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "type": "client-request",
        "rpcId": format!("p-{}", std::process::id()),
        "method": "session/prompt",
        "payload": { "args": { "request": {
            "requestId": format!("req-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)),
            "sessionId": cfg.session,
            "mode": "queue",
            "content": [{ "type": "text", "text": text }]
        } } }
    });
    let r = client.post(format!("{}/api/session/prompt", cfg.origin))
        .header("Cookie", &cfg.cookie)
        .header("Content-Type", "application/json")
        .header("Origin", &cfg.origin)
        .body(body.to_string())
        .send().await?;
    let status = r.status();
    let body = r.text().await.unwrap_or_default();
    eprintln!("prompt -> {status} {}", &body[..body.len().min(120)]);
    Ok(())
}

async fn rpc(cfg: &Cfg, method: &str, args: Value) -> Result<Value> {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "type": "client-request",
        "rpcId": format!("ui-{}", std::process::id()),
        "method": method,
        "payload": { "args": args },
    });
    let resp = client.post(format!("{}/api/{method}", cfg.origin))
        .header("Cookie", &cfg.cookie)
        .header("Content-Type", "application/json")
        .header("Origin", &cfg.origin)
        .body(body.to_string())
        .send().await?;
    let text = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let result = &v["result"];
    if result["ok"].as_bool() == Some(true) {
        Ok(result["value"].clone())
    } else {
        Err(anyhow!("{}", result["error"]["message"].as_str().unwrap_or(&text[..text.len().min(160)])))
    }
}

async fn load_ui(cfg: &Cfg) -> UiState {
    let mut ui = UiState::default();
    match rpc(cfg, "permissionPresets/catalog", serde_json::json!({})).await {
        Ok(v) => {
            for opt in v["options"].as_array().cloned().unwrap_or_default() {
                let value = opt["value"].as_str().unwrap_or("").to_string();
                if value.is_empty() { continue; }
                let name = opt["name"].as_str().unwrap_or(&value).to_string();
                ui.perm_options.push((value, name));
            }
        }
        Err(e) => eprintln!("permission catalog: {e}"),
    }
    match rpc(cfg, "session/projections", serde_json::json!({ "request": { "sessionId": cfg.session } })).await {
        Ok(v) => {
            let values = &v["values"];
            ui.perm_current = values["permissions"]["currentValue"].as_str().unwrap_or("").to_string();
            let next = &values["modelSelection"]["next"];
            if let (Some(provider), Some(model)) = (next["provider"].as_str(), next["model"].as_str()) {
                ui.sel = Some(Sel {
                    provider: provider.to_string(),
                    model: model.to_string(),
                    effort: next["reasoningEffort"].as_str().map(|s| s.to_string()),
                });
            }
        }
        Err(e) => eprintln!("projections: {e}"),
    }
    match rpc(cfg, "session/modelCatalog", serde_json::json!({})).await {
        Ok(v) => {
            for group in v["groups"].as_array().cloned().unwrap_or_default() {
                let gid = group["id"].as_str().unwrap_or("").to_string();
                let gname = group["name"].as_str().unwrap_or(&gid).to_string();
                let mut models = Vec::new();
                for m in group["models"].as_array().cloned().unwrap_or_default() {
                    let id = m["id"].as_str().unwrap_or("").to_string();
                    let name = m["name"].as_str().unwrap_or(&id).to_string();
                    let mut efforts = Vec::new();
                    for e in m["reasoning"]["efforts"].as_array().cloned().unwrap_or_default() {
                        let eid = e["id"].as_str().unwrap_or("").to_string();
                        let ename = e["name"].as_str().unwrap_or(&eid).to_string();
                        efforts.push((eid, ename));
                    }
                    models.push(ModelEntry { provider: gid.clone(), id, name, efforts });
                }
                ui.groups.push((gid, gname, models));
            }
        }
        Err(e) => eprintln!("model catalog: {e}"),
    }
    eprintln!("ui: permission={} models={} groups sel={:?}", ui.perm_current, ui.groups.iter().map(|g| g.2.len()).sum::<usize>(), ui.sel);
    ui
}

#[derive(Clone)]
enum UiEvent { Permission(String), Sel(Sel) }

/// Work started for the left pane that has to come back to the UI thread.
enum NavEvent { HeroSession(String), Sidebar(Box<Sidebar>) }

struct Shell {
    title: String,
    cfg: Cfg,
    input: String,
    caret: usize,
    preedit: String,
    focused: bool,
    hover: bool,
    caret_node: Option<blitz_dom::NodeId>,
    proxy: BlitzShellProxy,
    events: std::sync::mpsc::Receiver<blitz_shell::BlitzShellEvent>,
    view: Option<blitz_shell::View<VelloCpuWindowRenderer>>,
    updates: std::sync::mpsc::Receiver<(String, String, String)>,
    page: String,
    #[allow(dead_code)]
    last_seq: i64,
    last_items: String,
    ui: UiState,
    ui_tx: std::sync::mpsc::Sender<UiEvent>,
    ui_rx: std::sync::mpsc::Receiver<UiEvent>,
    clicktest: Vec<String>,
    clickat: std::time::Instant,
    sidebar: Sidebar,
    nav_tx: std::sync::mpsc::Sender<NavEvent>,
    nav_rx: std::sync::mpsc::Receiver<NavEvent>,
    session_tx: tokio::sync::watch::Sender<String>,
    pointer: (f64, f64),
    hero: Hero,
    /// One font database for every document this window builds.
    font_ctx: parley::FontContext,
    #[allow(dead_code)]
    /// Coalesced transcript update still waiting for the rebuild budget.
    pending: Option<String>,
    last_apply: std::time::Instant,
    chrome: String,
    stats: Stats,
    stats_rx: std::sync::mpsc::Receiver<Stats>,
}

impl Shell {
    fn insert_str(&mut self, s: &str) {
        self.input.insert_str(self.caret, s);
        self.caret += s.len();
    }
    fn backspace(&mut self) {
        if self.caret == 0 { return; }
        let prev = self.input[..self.caret].char_indices().last().map(|(i, _)| i).unwrap_or(0);
        self.input.replace_range(prev..self.caret, "");
        self.caret = prev;
    }
    fn delete_forward(&mut self) {
        if self.caret >= self.input.len() { return; }
        let next = self.input[self.caret..].char_indices().nth(1).map(|(i, _)| self.caret + i).unwrap_or(self.input.len());
        self.input.replace_range(self.caret..next, "");
    }
    fn move_caret_left(&mut self) {
        if self.caret > 0 {
            self.caret = self.input[..self.caret].char_indices().last().map(|(i, _)| i).unwrap_or(0);
        }
    }
    fn move_caret_right(&mut self) {
        if self.caret < self.input.len() {
            self.caret = self.input[self.caret..].char_indices().nth(1).map(|(i, _)| self.caret + i).unwrap_or(self.input.len());
        }
    }

    fn input_rect(&mut self) -> Option<(f32, f32, f32, f32)> {
        let view = self.view.as_mut()?;
        let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
        let id = doc.get_element_by_id("composer-input")?;
        let mut x = 0.0f32;
        let mut y = 0.0f32;
        let (mut w, mut h) = (0.0f32, 0.0f32);
        let mut cur = Some(id);
        while let Some(n) = cur {
            if let Some(node) = doc.get_node(n) {
                if node.is_element() {
                    let l = node.final_layout();
                    x += l.location.x;
                    y += l.location.y;
                    if n == id { w = l.size.width; h = l.size.height; }
                }
            }
            cur = doc.mutate().parent_id(n);
        }
        Some((x, y, w, h))
    }

    fn hit_composer(&mut self, pos: winit::dpi::PhysicalPosition<f64>) -> bool {
        let Some(view) = self.view.as_mut() else { return false };
        let coords = view.pointer_coords(pos);
        let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
        let Some(card) = doc.get_element_by_id("composer-card") else { return false };
        let Some(hit) = doc.element_from_point(coords.page_x, coords.page_y) else { return false };
        let mut cur = Some(hit);
        while let Some(id) = cur {
            if id == card { return true; }
            cur = doc.mutate().parent_id(id);
        }
        false
    }

    fn caret_from_click(&mut self, pos: winit::dpi::PhysicalPosition<f64>) -> usize {
        let Some(view) = self.view.as_mut() else { return self.caret };
        let coords = view.pointer_coords(pos);
        let (x0, _, _, _) = match self.input_rect() { Some(r) => r, None => return self.caret };
        let rel = (coords.page_x - x0).max(0.0);
        let em = 16.0f32;
        let mut acc = 0.0f32;
        for (i, ch) in self.input.char_indices() {
            let cw = if is_wide(ch) { em } else { em * 0.5 };
            if rel < acc + cw * 0.5 { return i; }
            if rel < acc + cw { return i + ch.len_utf8(); }
            acc += cw;
        }
        self.input.len()
    }

    fn ime_cursor_area(&mut self) {
        let Some((x, y, _, h)) = self.input_rect() else { return };
        let Some(view) = self.view.as_mut() else { return };
        {
            let scale = 1.5f64;
            let _ = view.window.request_ime_update(winit::window::ImeRequest::Update(
                winit::window::ImeRequestData::default().with_cursor_area(
                    winit::dpi::PhysicalPosition::new(x as f64 * scale, (y + h) as f64 * scale).into(),
                    winit::dpi::PhysicalSize::new(1.0, 20.0).into(),
                ),
            ));
        }
    }

    fn render_composer(&mut self) {
        let Some(view) = self.view.as_mut() else { return };
        let focused = self.focused;
        self.caret = clamp_caret(&self.input, self.caret);
        let before = self.input[..self.caret].to_string();
        let after = self.input[self.caret..].to_string();
        let preedit = self.preedit.clone();
        let mut caret_node = None;
        {
            use blitz_dom::{local_name, ns, Attribute, QualName};
            let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
            let Some(node) = doc.get_element_by_id("composer-input") else { return };
            if !self.input.is_empty() || !preedit.is_empty() {
                if let Some(ph) = doc.get_element_by_id("composer-placeholder") {
                    doc.mutate().remove_node(ph);
                }
            }
            let kids = doc.mutate().child_ids(node);
            for k in kids { doc.mutate().remove_node(k); }
            {
                let mut m = doc.mutate();
                let mut nodes = Vec::new();
                if !before.is_empty() { nodes.push(m.create_text_node(&before)); }
                if focused {
                    let c = m.create_element(
                        QualName::new(None, ns!(html), local_name!("span")),
                        vec![Attribute {
                            name: QualName::new(None, ns!(), local_name!("style")),
                            value: "display:inline-block;width:0;height:1.15em;border-left:1px solid currentColor;margin-left:-1px;padding:0;vertical-align:text-bottom".to_string(),
                        }],
                    );
                    caret_node = Some(c);
                    nodes.push(c);
                }
                if !preedit.is_empty() { nodes.push(m.create_text_node(&preedit)); }
                if !after.is_empty() { nodes.push(m.create_text_node(&after)); }
                if !nodes.is_empty() { m.append_children(node, &nodes); }
            }
        }
        self.caret_node = caret_node;
        view.poll();
        view.request_redraw();
        if focused { self.ime_cursor_area(); }
    }

    fn send_input(&mut self) {
        if self.ui.menu != Menu::None { self.ui.menu = Menu::None; }
        let text = std::mem::take(&mut self.input);
        self.caret = 0;
        self.preedit.clear();
        self.render_composer();
        if text.trim().is_empty() { return; }
        if self.hero.active {
            // The hero composer is inert until a workspace is chosen (DSH parity).
            if self.hero.session.is_none() {
                return;
            }
            self.hero.active = false;
            self.title = self.sidebar.title_of(&self.cfg.session);
        }
        let cfg = self.cfg.clone();
        std::thread::spawn(move || {
            let rt = rt_current().unwrap();
            let _ = rt.block_on(post_prompt(&cfg, &text));
        });
    }

    fn scroll_metrics(&mut self) -> Option<(f64, f64)> {
        let view = self.view.as_mut()?;
        let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
        let id = doc.get_element_by_id("conv-scroll")?;
        let node = doc.get_node(id)?;
        let offset = node.scroll_offset().y;
        let max = (node.scrollable_overflow().y1 - node.final_layout().size.height as f64).max(0.0);
        Some((offset, max))
    }

    fn set_scroll(&mut self, y: f64) {
        let Some(view) = self.view.as_mut() else { return };
        {
            let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
            if let Some(id) = doc.get_element_by_id("conv-scroll") {
                if let Some(node) = doc.get_node_mut(id) { node.scroll_offset_mut().y = y.max(0.0); }
            }
        }
        view.request_redraw();
    }

    fn scroll_to_bottom(&mut self) {
        let Some(view) = self.view.as_mut() else { return };
        {
            let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
            if let Some(id) = doc.get_element_by_id("conv-scroll") {
                let max = {
                    let node = doc.get_node(id).unwrap();
                    let overflow = *node.scrollable_overflow();
                    let h = node.final_layout().size.height as f64;
                    (overflow.y1 - h).max(0.0)
                };
                if std::env::var("DSH_DEBUG").is_ok() {
                    eprintln!("scroll: max={max:.1}");
                }
                if let Some(node) = doc.get_node_mut(id) {
                    node.scroll_offset_mut().y = max;
                }
            }
        }
        view.request_redraw();
    }

    fn render_live(&mut self, text: &str, think: &str) {
        let Some(view) = self.view.as_mut() else { return };
        if std::env::var("DSH_DEBUG").is_ok() { eprintln!("live: text={} think={}", text.chars().count(), think.chars().count()); }
        {
            let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
            for (id, val) in [("live-tail", text), ("live-think", think)] {
                if let Some(node) = doc.get_element_by_id(id) {
                    let kids = doc.mutate().child_ids(node);
                    for k in kids { doc.mutate().remove_node(k); }
                    if !val.is_empty() {
                        let mut m = doc.mutate();
                        let t = m.create_text_node(val);
                        m.append_children(node, &[t]);
                    }
                }
            }
        }
        view.poll();
        view.request_redraw();
    }

    fn hit_id(&mut self, pos: winit::dpi::PhysicalPosition<f64>) -> Option<String> {
        use blitz_dom::local_name;
        let view = self.view.as_mut()?;
        let coords = view.pointer_coords(pos);
        let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
        let hit = doc.element_from_point(coords.page_x, coords.page_y)?;
        let mut cur = Some(hit);
        while let Some(n) = cur {
            if let Some(node) = doc.get_node(n) {
                if node.is_element() {
                    if let Some(id) = node.attr(local_name!("id")) { return Some(id.to_string()); }
                }
            }
            cur = doc.mutate().parent_id(n);
        }
        None
    }

    fn resolve_at_center(&mut self, id: &str) -> Option<String> {
        use blitz_dom::local_name;
        let view = self.view.as_mut()?;
        let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
        let node = doc.get_element_by_id(id)?;
        let (mut x, mut y) = (0.0f64, 0.0f64);
        let mut cur = Some(node);
        while let Some(n) = cur {
            if let Some(nd) = doc.get_node(n) {
                let l = nd.final_layout();
                x += l.location.x as f64;
                y += l.location.y as f64;
            }
            cur = doc.mutate().parent_id(n);
        }
        let (w, h) = { let nd = doc.get_node(node)?; (nd.final_layout().size.width as f64, nd.final_layout().size.height as f64) };
        let hit = doc.element_from_point((x + w / 2.0) as f32, (y + h / 2.0) as f32)?;
        let mut cur = Some(hit);
        while let Some(n) = cur {
            if let Some(nd) = doc.get_node(n) {
                if nd.is_element() {
                    if let Some(v) = nd.attr(local_name!("id")) { return Some(v.to_string()); }
                }
            }
            cur = doc.mutate().parent_id(n);
        }
        None
    }

    fn repaint(&mut self) {
        let items = self.last_items.clone();
        self.apply(&items);
    }

    /// Toggle which pre-rendered popup is visible. Only inline styles change, so
    /// this costs a restyle instead of a full document rebuild.
    fn update_menu_visibility(&mut self) {
        let open = self.ui.menu;
        let Some(view) = self.view.as_mut() else { return };
        {
            let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
            let mut updates: Vec<(blitz_dom::NodeId, &str, &str)> = Vec::new();
            for (id, menu) in [
                ("menu-permission", Menu::Permission),
                ("menu-model", Menu::Model),
                ("menu-model-list", Menu::ModelList),
                ("menu-effort", Menu::Effort),
            ] {
                let shown = if open == menu { "flex" } else { "none" };
                if let Some(node) = doc.get_element_by_id(id) { updates.push((node, "display", shown)); }
            }
            if let Some(node) = doc.get_element_by_id("perm-chevron") {
                let v = if open == Menu::Permission { "rotate(180deg)" } else { "none" };
                updates.push((node, "transform", v));
            }
            if let Some(node) = doc.get_element_by_id("model-chevron") {
                let v = if matches!(open, Menu::Model | Menu::ModelList | Menu::Effort) { "rotate(180deg)" } else { "none" };
                updates.push((node, "transform", v));
            }
            let mut m = doc.mutate();
            for (node, prop, value) in updates {
                m.set_style_property(node, prop, value);
            }
        }
        let started = std::time::Instant::now();
        {
            let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
            doc.resolve(0.0);
            doc.resolve(0.0);
        }
        view.poll();
        view.request_redraw();
        eprintln!("menu: {:?} in {:?}", self.ui.menu, started.elapsed());
    }

    fn toggle_menu(&mut self, menu: Menu) {
        self.ui.menu = if self.ui.menu == menu { Menu::None } else { menu };
        self.update_menu_visibility();
    }


    /// Collapse or expand the left pane. Only inline styles change, so the
    /// document is not rebuilt.
    fn set_sidebar_expanded(&mut self, on: bool) {
        if self.sidebar.expanded == on {
            return;
        }
        self.sidebar.expanded = on;
        let started = std::time::Instant::now();
        let Some(view) = self.view.as_mut() else { return };
        {
            let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
            let mut updates: Vec<(blitz_dom::NodeId, &str, &str)> = Vec::new();
            if let Some(n) = doc.get_element_by_id("sidebar-rail") {
                updates.push((n, "display", if on { "none" } else { "flex" }));
            }
            if let Some(n) = doc.get_element_by_id("sidebar-full") {
                updates.push((n, "display", if on { "flex" } else { "none" }));
            }
            if let Some(n) = doc.get_element_by_id("app-frame") {
                updates.push((
                    n,
                    "grid-template-columns",
                    if on { "280px minmax(0px, 1fr) minmax(0px, 0px)" } else { "56px minmax(0px, 1fr) minmax(0px, 0px)" },
                ));
            }
            let mut m = doc.mutate();
            for (node, prop, value) in updates {
                m.set_style_property(node, prop, value);
            }
        }
        {
            let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
            doc.resolve(0.0);
            doc.resolve(0.0);
        }
        view.poll();
        view.request_redraw();
        if std::env::var("DSH_DEBUG").is_ok() {
            let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
            for id in ["app-frame", "sidebar-full", "sidebar-rail", "composer-card"] {
                if let Some(n) = doc.get_element_by_id(id) {
                    if let Some(node) = doc.get_node(n) {
                        let l = node.final_layout();
                        eprintln!("  {id}: {:.0}x{:.0} at x={:.0}", l.size.width, l.size.height, l.location.x);
                    }
                }
            }
        }
        eprintln!("sidebar: expanded={on} in {:?}", started.elapsed());
        if on {
            self.refresh_sidebar();
        }
    }

    /// Fold a Workspace group open or shut.
    fn toggle_group(&mut self, idx: usize) {
        let Some(key) = self.sidebar.groups().get(idx).map(|g| g.0.clone()) else { return };
        let open = !self.sidebar.collapsed.contains(&key);
        if open {
            self.sidebar.collapsed.insert(key);
        } else {
            self.sidebar.collapsed.remove(&key);
        }
        let Some(view) = self.view.as_mut() else { return };
        {
            let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
            let mut rows: Vec<blitz_dom::NodeId> = Vec::new();
            if let Some(sec) = doc.get_element_by_id(&format!("sb-sec-{idx}")) {
                rows.extend(doc.mutate().child_ids(sec).into_iter().skip(1));
            }
            let mut glyph: Vec<(blitz_dom::NodeId, &str)> = Vec::new();
            if let Some(n) = doc.get_element_by_id(&format!("sb-foldo-{idx}")) {
                glyph.push((n, if open { "none" } else { "contents" }));
            }
            if let Some(n) = doc.get_element_by_id(&format!("sb-foldc-{idx}")) {
                glyph.push((n, if open { "contents" } else { "none" }));
            }
            let display = if open { "none" } else { "block" };
            let mut m = doc.mutate();
            for n in rows {
                m.set_style_property(n, "display", display);
            }
            for (n, v) in glyph {
                m.set_style_property(n, "display", v);
            }
        }
        {
            let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
            doc.resolve(0.0);
            doc.resolve(0.0);
        }
        view.poll();
        view.request_redraw();
    }

    /// Re-read the Workspace/Session lists off the UI thread.
    fn refresh_sidebar(&mut self) {
        let cfg = self.cfg.clone();
        let tx = self.nav_tx.clone();
        std::thread::spawn(move || {
            let rt = rt_current().unwrap();
            let sb = rt.block_on(load_sidebar(&cfg));
            let _ = tx.send(NavEvent::Sidebar(Box::new(sb)));
        });
    }

    /// Follow another Session: retarget the stream and rebuild the transcript.
    fn switch_session(&mut self, id: String) {
        if id == self.cfg.session {
            return;
        }
        eprintln!("switch: {} -> {id}", self.cfg.session);
        self.hero.active = false;
        self.hero.session = None;
        self.cfg.session = id.clone();
        self.title = self.sidebar.title_of(&id);
        self.sidebar.current = id.clone();
        let _ = self.session_tx.send(id);
        self.last_items.clear();
        self.ui.menu = Menu::None;
        self.apply("");
        self.scroll_to_bottom();
        let cfg = self.cfg.clone();
        let tx = self.ui_tx.clone();
        std::thread::spawn(move || {
            let rt = rt_current().unwrap();
            let ui = rt.block_on(load_ui(&cfg));
            let _ = tx.send(UiEvent::Permission(ui.perm_current));
            if let Some(sel) = ui.sel {
                let _ = tx.send(UiEvent::Sel(sel));
            }
        });
        self.refresh_sidebar();
    }

    /// New Session opens DSH's empty-task hero: no Session yet, so the workspace
    /// chip decides where the first prompt lands.
    fn start_hero(&mut self, workspace_id: Option<String>) {
        self.hero.active = true;
        self.hero.session = None;
        self.hero.workspace = workspace_id
            .or_else(|| self.sidebar.workspace_of(&self.cfg.session))
            .or_else(|| self.sidebar.workspaces.first().map(|w| w.id.clone()));
        if self.hero.presets.is_empty() {
            self.hero.presets = load_presets_now(&self.cfg);
        }
        if self.hero.preset.is_none() {
            self.hero.preset = self.hero.presets.iter().find(|p| p.id == "ptc").map(|p| p.id.clone());
        }
        self.hero.menu = HeroMenu::None;
        self.title = "New Session".to_string();
        self.last_items.clear();
        self.ui.menu = Menu::None;
        self.apply("");
        self.focused = true;
        self.caret = self.input.len();
        self.render_composer();
    }

    /// Follow a Session created from the hero without closing the hero.
    fn switch_session_into_hero(&mut self, id: String) {
        let keep_hero = self.hero.clone();
        self.switch_session(id);
        self.hero = keep_hero;
        self.hero.active = true;
        self.hero.session = Some(self.cfg.session.clone());
        self.title = "New Session".to_string();
        let items = self.last_items.clone();
        self.apply(&items);
    }

    /// A workspace pick creates (or reuses) the blank Session inside it.
    fn pick_workspace(&mut self, id: String) {
        self.hero.workspace = Some(id.clone());
        self.hero.menu = HeroMenu::None;
        let cfg = self.cfg.clone();
        let tx = self.nav_tx.clone();
        let preset = self.hero.preset.clone();
        let reuse = self.hero.session.clone();
        std::thread::spawn(move || {
            let rt = rt_current().unwrap();
            let mut req = serde_json::Map::new();
            req.insert("workspaceId".to_string(), serde_json::json!(id));
            if let Some(p) = preset {
                req.insert("agentPreset".to_string(), serde_json::json!(p));
            }
            if let Some(s) = reuse {
                req.insert("sessionId".to_string(), serde_json::json!(s));
            }
            let args = serde_json::json!({ "request": Value::Object(req) });
            match rt.block_on(rpc(&cfg, "session/create", args)) {
                Ok(v) => {
                    if let Some(sid) = v["sessionId"].as_str() {
                        let _ = tx.send(NavEvent::HeroSession(sid.to_string()));
                    }
                }
                Err(e) => eprintln!("create session: {e}"),
            }
        });
    }

    /// A preset pick re-targets the blank Session through the preset registry.
    fn pick_preset(&mut self, id: String) {
        self.hero.preset = Some(id.clone());
        self.hero.menu = HeroMenu::None;
        if let Some(session) = self.hero.session.clone() {
            let cfg = self.cfg.clone();
            std::thread::spawn(move || {
                let rt = rt_current().unwrap();
                let args = serde_json::json!({ "agentId": session, "agentPreset": id });
                match rt.block_on(rpc(&cfg, "agentPresets/select", args)) {
                    Ok(_) => eprintln!("preset selected"),
                    Err(e) => eprintln!("preset select: {e}"),
                }
            });
        }
        let items = self.last_items.clone();
        self.apply(&items);
    }

    /// Flip which hero popup is visible without rebuilding the page.
    fn set_hero_menu(&mut self, menu: HeroMenu) {
        self.hero.menu = menu;
        let Some(view) = self.view.as_mut() else { return };
        {
            let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
            let mut updates: Vec<(blitz_dom::NodeId, &str, &str)> = Vec::new();
            for (id, kind) in [("hero-menu-ws", HeroMenu::Workspace), ("hero-menu-preset", HeroMenu::Preset)] {
                if let Some(node) = doc.get_element_by_id(id) {
                    updates.push((node, "display", if menu == kind { "block" } else { "none" }));
                }
            }
            let mut m = doc.mutate();
            for (node, prop, value) in updates {
                m.set_style_property(node, prop, value);
            }
        }
        {
            let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
            doc.resolve(0.0);
            doc.resolve(0.0);
        }
        view.poll();
        view.request_redraw();
    }

    /// Rewrite the composer dock readings in place.
    fn render_stats(&mut self) {
        let stats = self.stats.clone();
        if std::env::var("DSH_DEBUG").is_ok() {
            eprintln!(
                "stats: [{}] [{}] [{}] [{}] [{}] ring={}",
                stats.activity, stats.speed, stats.usage, stats.cache, stats.context, stats.ring
            );
        }
        let Some(view) = self.view.as_mut() else { return };
        {
            let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
            for (id, val) in [
                ("stat-activity-a", &stats.activity),
                ("stat-activity-b", &stats.speed),
                ("stat-usage-a", &stats.usage),
                ("stat-usage-b", &stats.cache),
                ("stat-context", &stats.context),
            ] {
                let Some(node) = doc.get_element_by_id(id) else { continue };
                let kids = doc.mutate().child_ids(node);
                for k in kids {
                    doc.mutate().remove_node(k);
                }
                if !val.is_empty() {
                    let mut m = doc.mutate();
                    let t = m.create_text_node(val);
                    m.append_children(node, &[t]);
                }
            }
            if !stats.ring.is_empty() {
                if let Some(node) = doc.get_element_by_id("stat-context-ring") {
                    let ring = stats.ring.clone();
                    let mut m = doc.mutate();
                    m.set_style_property(node, "stroke-dasharray", &ring);
                }
            }
        }
        view.request_redraw();
    }

    /// Wheel over the Session tree scrolls that list, not the transcript.
    fn sidebar_wheel(&mut self, dy: f64) -> bool {
        let (px, py) = self.pointer;
        let Some(view) = self.view.as_mut() else { return false };
        let coords = view.pointer_coords(winit::dpi::PhysicalPosition::new(px, py));
        let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
        let Some(list) = doc.get_element_by_id("sb-scroll") else { return false };
        let Some(hit) = doc.element_from_point(coords.page_x, coords.page_y) else { return false };
        let mut inside = false;
        let mut cur = Some(hit);
        while let Some(n) = cur {
            if n == list {
                inside = true;
                break;
            }
            cur = doc.mutate().parent_id(n);
        }
        if !inside {
            return false;
        }
        if let Some(node) = doc.get_node_mut(list) {
            let offset = node.scroll_offset().y + dy;
            let max = (node.scrollable_overflow().y1 - node.final_layout().size.height as f64).max(0.0);
            node.scroll_offset_mut().y = offset.clamp(0.0, max);
        }
        view.request_redraw();
        true
    }

    fn on_chip_click(&mut self, id: &str) -> bool {
        match id {
            "sb-toggle-collapsed" | "sb-toggle" => {
                let on = !self.sidebar.expanded;
                self.set_sidebar_expanded(on);
                true
            }
            "sb-new-collapsed" | "sb-new" | "sb-brand" => {
                self.start_hero(None);
                true
            }
            "hero-ws" => {
                let next = if self.hero.menu == HeroMenu::Workspace { HeroMenu::None } else { HeroMenu::Workspace };
                self.set_hero_menu(next);
                true
            }
            "hero-preset" => {
                let next = if self.hero.menu == HeroMenu::Preset { HeroMenu::None } else { HeroMenu::Preset };
                self.set_hero_menu(next);
                true
            }
            "perm-trigger" => { self.toggle_menu(Menu::Permission); true }
            "model-trigger" => { self.toggle_menu(Menu::Model); true }
            "model-cell-model" => { self.ui.menu = Menu::ModelList; self.update_menu_visibility(); true }
            "model-cell-effort" => { self.ui.menu = Menu::Effort; self.update_menu_visibility(); true }
            _ => {
                if let Some(rest) = id.strip_prefix("hero-ws-opt-") {
                    if let Ok(i) = rest.parse::<usize>() {
                        if let Some(w) = self.sidebar.workspaces.get(i) {
                            let id = w.id.clone();
                            self.pick_workspace(id);
                            return true;
                        }
                    }
                }
                if let Some(rest) = id.strip_prefix("hero-preset-opt-") {
                    if let Ok(i) = rest.parse::<usize>() {
                        if let Some(p) = self.hero.presets.get(i) {
                            let id = p.id.clone();
                            self.pick_preset(id);
                            return true;
                        }
                    }
                }
                if let Some(rest) = id.strip_prefix("sb-s-") {
                    self.switch_session(rest.to_string());
                    return true;
                }
                if let Some(rest) = id.strip_prefix("sb-ws-") {
                    if let Ok(i) = rest.parse::<usize>() {
                        self.toggle_group(i);
                        return true;
                    }
                }
                if let Some(rest) = id.strip_prefix("sb-newws-") {
                    if let Ok(i) = rest.parse::<usize>() {
                        let ws = self.sidebar.groups().get(i).map(|g| g.0.clone()).filter(|k| !k.is_empty());
                        self.start_hero(ws);
                        return true;
                    }
                }
                if let Some(rest) = id.strip_prefix("sb-more-") {
                    if let Ok(i) = rest.parse::<usize>() {
                        if let Some(key) = self.sidebar.groups().get(i).map(|g| g.0.clone()) {
                            self.sidebar.show_all.insert(key);
                            let items = self.last_items.clone();
                            self.apply(&items);
                            return true;
                        }
                    }
                }
                if let Some(rest) = id.strip_prefix("perm-opt-") {
                    if let Ok(i) = rest.parse::<usize>() {
                        if let Some((value, _)) = self.ui.perm_options.get(i).cloned() {
                            self.apply_permission(value);
                            return true;
                        }
                    }
                }
                if let Some(rest) = id.strip_prefix("model-opt-") {
                    let mut it = rest.split('-');
                    if let (Some(gi), Some(mi)) = (it.next(), it.next()) {
                        if let (Ok(gi), Ok(mi)) = (gi.parse::<usize>(), mi.parse::<usize>()) {
                            if let Some((_, _, models)) = self.ui.groups.get(gi) {
                                if let Some(m) = models.get(mi) {
                                    let sel = Sel { provider: m.provider.clone(), model: m.id.clone(), effort: None };
                                    self.apply_model(sel);
                                    return true;
                                }
                            }
                        }
                    }
                }
                if let Some(rest) = id.strip_prefix("effort-opt-") {
                    if let Ok(i) = rest.parse::<usize>() {
                        if let Some(cur) = self.ui.sel.clone() {
                            let effort = if i == 0 {
                                None
                            } else {
                                self.ui.current_model()
                                    .and_then(|m| m.efforts.get(i - 1))
                                    .map(|(eid, _)| eid.clone())
                            };
                            self.apply_model(Sel { provider: cur.provider, model: cur.model, effort });
                            return true;
                        }
                    }
                }
                false
            }
        }
    }

    fn apply_permission(&mut self, value: String) {
        self.ui.menu = Menu::None;
        self.update_menu_visibility();
        let cfg = self.cfg.clone();
        let tx = self.ui_tx.clone();
        std::thread::spawn(move || {
            let rt = rt_current().unwrap();
            rt.block_on(async {
                let args = serde_json::json!({
                    "agentId": cfg.session,
                    "line": format!("/permission {value}"),
                    "submittedAttachments": [],
                });
                match rpc(&cfg, "commands/execute", args).await {
                    Ok(v) => {
                        let text = v["result"]["text"].as_str().unwrap_or("").to_string();
                        eprintln!("permission -> {text}");
                        if let Ok(p) = rpc(&cfg, "session/projections", serde_json::json!({ "request": { "sessionId": cfg.session } })).await {
                            if let Some(cur) = p["values"]["permissions"]["currentValue"].as_str() {
                                let _ = tx.send(UiEvent::Permission(cur.to_string()));
                            }
                        }
                    }
                    Err(e) => eprintln!("permission -> error: {e}"),
                }
            });
        });
    }

    fn apply_model(&mut self, sel: Sel) {
        self.ui.menu = Menu::None;
        self.update_menu_visibility();
        let cfg = self.cfg.clone();
        let tx = self.ui_tx.clone();
        std::thread::spawn(move || {
            let rt = rt_current().unwrap();
            rt.block_on(async {
                let mut request = serde_json::json!({
                    "sessionId": cfg.session,
                    "provider": sel.provider,
                    "model": sel.model,
                });
                if let Some(effort) = sel.effort.as_ref() {
                    request["reasoningEffort"] = serde_json::json!(effort);
                }
                match rpc(&cfg, "session/selectModel", serde_json::json!({ "request": request })).await {
                    Ok(v) => {
                        let s = &v["selected"];
                        let applied = Sel {
                            provider: s["provider"].as_str().unwrap_or(&sel.provider).to_string(),
                            model: s["model"].as_str().unwrap_or(&sel.model).to_string(),
                            effort: s["reasoningEffort"].as_str().map(|x| x.to_string()),
                        };
                        let _ = tx.send(UiEvent::Sel(applied));
                    }
                    Err(e) => eprintln!("selectModel -> error: {e}"),
                }
            });
        });
    }

    fn node_page_y(&mut self, id: &str) -> Option<f64> {
        let view = self.view.as_mut()?;
        let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
        let mut node = doc.get_element_by_id(id)?;
        let mut y = 0.0f64;
        loop {
            if let Some(nd) = doc.get_node(node) {
                y += nd.final_layout().location.y as f64;
            }
            match doc.mutate().parent_id(node) { Some(p) => node = p, None => break }
        }
        Some(y)
    }

    fn scroll_to_selected(&mut self) {
        let mut found = None;
        for (gi, (_, _, models)) in self.ui.groups.iter().enumerate() {
            for mi in 0..models.len() {
                if let Some(sel) = self.ui.sel.as_ref() {
                    if models[mi].provider == sel.provider && models[mi].id == sel.model { found = Some((gi, mi)); }
                }
            }
        }
        let Some((gi, mi)) = found else { return };
        let id = format!("model-opt-{gi}-{mi}");
        let (Some(item_y), Some(list_y)) = (self.node_page_y(&id), self.node_page_y("model-list")) else { return };
        let Some(view) = self.view.as_mut() else { return };
        {
            let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
            if let Some(list) = doc.get_element_by_id("model-list") {
                let max = {
                    let Some(node) = doc.get_node(list) else { return };
                    (node.scrollable_overflow().y1 - node.final_layout().size.height as f64).max(0.0)
                };
                let target = (item_y - list_y - 90.0).clamp(0.0, max);
                if let Some(node) = doc.get_node_mut(list) { node.scroll_offset_mut().y = target; }
            }
        }
        view.request_redraw();
    }

    fn scroll_menu_list(&mut self, dy: f64) {
        let Some(view) = self.view.as_mut() else { return };
        {
            let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
            let id = doc.get_element_by_id("model-list").or_else(|| doc.get_element_by_id("effort-list"));
            if let Some(id) = id {
                let (max, cur) = {
                    let Some(node) = doc.get_node(id) else { return };
                    let max = (node.scrollable_overflow().y1 - node.final_layout().size.height as f64).max(0.0);
                    (max, node.scroll_offset().y)
                };
                let next = (cur + dy).clamp(0.0, max);
                if let Some(node) = doc.get_node_mut(id) { node.scroll_offset_mut().y = next; }
            }
        }
        view.request_redraw();
    }

    /// Signature of everything in the page except the transcript. When it is
    /// unchanged the document survives and only the conversation is rewritten.
    fn chrome_key(&self) -> String {
        format!(
            "{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}",
            self.title,
            self.sidebar.html(),
            self.hero.html(&self.sidebar.workspaces),
            self.ui.model_name(),
            self.ui.effort_label(),
            self.ui.perm_current,
            self.sidebar.expanded,
        )
    }

    fn apply(&mut self, items: &str) {
        if self.view.is_none() { return; }
        self.last_apply = std::time::Instant::now();
        self.last_items = items.to_string();
        let started = std::time::Instant::now();
        let (prev_offset, prev_max) = self.scroll_metrics().unwrap_or((0.0, 0.0));
        let was_following = prev_offset >= prev_max - 25.0;
        let chrome = self.chrome_key();
        if chrome == self.chrome && std::env::var("DSH_INCR").is_ok() {
            // Transcript-only change: rewrite the flow column in place instead
            // of reparsing the whole page.
            {
                let view = self.view.as_mut().unwrap();
                let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
                if let Some(host) = doc.get_element_by_id("chat-col") {
                    doc.mutate().set_inner_html(host, items);
                }
                doc.resolve(0.0);
                view.request_redraw();
            }
            self.render_composer();
            if was_following { self.scroll_to_bottom(); } else { self.set_scroll(prev_offset); }
            eprintln!("apply(items): {} bytes in {:?} following={was_following}", items.len(), started.elapsed());
            return;
        }
        let page = build_page(items, &self.title, &self.ui, &self.sidebar, &self.hero);
        {
            let view = self.view.as_mut().unwrap();
            let mut size = (1440u32, 900u32);
            view.with_viewport(|vp| size = vp.window_size);
            let doc = HtmlDocument::from_html(&page, DocumentConfig {
                base_url: Some(format!("file://{}", out_page())),
                viewport: Some(Viewport::new(size.0, size.1, 1.0, ColorScheme::Light)),
                // Needed by set_inner_html, which the transcript fast path uses.
                html_parser_provider: Some(std::sync::Arc::new(blitz_html::HtmlProvider)),
                font_ctx: Some(self.font_ctx.clone()),
                ..Default::default()
            });
            view.replace_document(Box::new(doc) as _, true);
            view.request_redraw();
        }
        self.chrome = chrome;
        self.render_composer();
        if self.ui.menu == Menu::ModelList { self.scroll_to_selected(); }
        if was_following { self.scroll_to_bottom(); } else { self.set_scroll(prev_offset); }
        // Hand freed page memory back to the OS after each rebuild.
        unsafe { malloc_trim(0) };
        eprintln!("apply: {} bytes in {:?} following={was_following}", items.len(), started.elapsed());
    }
}

impl ApplicationHandler for Shell {
    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        let (w, h) = (1440u32, 900u32);
        let doc = HtmlDocument::from_html(&self.page, DocumentConfig {
            base_url: Some(format!("file://{}", out_page())),
            viewport: Some(Viewport::new(w, h, 1.0, ColorScheme::Light)),
            ..Default::default()
        });
        let attrs = WindowAttributes::default()
            .with_title("dsh-blitz")
            .with_surface_size(LogicalSize::new(w as f64, h as f64));
        let config = WindowConfig::with_attributes(Box::new(doc) as _, VelloCpuWindowRenderer::new(), attrs);
        let mut view = blitz_shell::View::init(config, event_loop, &self.proxy);
        view.resume();
        self.view = Some(view);
        if let Some(v) = self.view.as_ref() { v.window.set_ime_allowed(true); }
        self.scroll_to_bottom();
        unsafe { malloc_trim(0) };
        if let Ok(n) = std::env::var("DSH_STRESS") {
            let n: usize = n.parse().unwrap_or(0);
            let items = self.last_items.clone();
            let started = std::time::Instant::now();
            for _ in 0..n {
                self.apply(&items);
            }
            eprintln!("stress: {n} applies in {:?}", started.elapsed());
            if let Ok(out) = std::env::var("DSH_SNAP") {
                use anyrender::PaintScene as _;
                let view = self.view.as_mut().unwrap();
                let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
                doc.resolve(0.0);
                doc.resolve(0.0);
                let (w, h) = (1440u32, 900u32);
                let buffer = anyrender::render_to_buffer::<anyrender_vello_cpu::VelloCpuImageRenderer, _>(
                    |scene| {
                        use peniko::{kurbo::Rect, Fill};
                        scene.fill(Fill::NonZero, Default::default(), blitz_dom::util::Color::WHITE, Default::default(), &Rect::new(0.0, 0.0, w as f64, h as f64));
                        blitz_paint::paint_scene(scene, doc.as_mut(), 1.0, w, h, 0, 0);
                    },
                    w,
                    h,
                );
                if let Ok(mut f) = std::fs::File::create(&out) {
                    let mut encoder = png::Encoder::new(&mut f, w, h);
                    encoder.set_color(png::ColorType::Rgba);
                    encoder.set_depth(png::BitDepth::Eight);
                    if let Ok(mut writer) = encoder.write_header() {
                        let _ = writer.write_image_data(&buffer);
                    }
                    eprintln!("snap: wrote {out}");
                }
            }
            if let Some(view) = self.view.as_mut() {
                let doc = view.downcast_doc_mut::<blitz_html::HtmlDocument>();
                if let Some(id) = doc.get_element_by_id("chat-col") {
                    let kids = doc.mutate().child_ids(id).len();
                    if let Some(node) = doc.get_node(id) {
                        let l = node.final_layout();
                        eprintln!(
                            "stress: chat-col {:.0}x{:.0} children={kids} overflow={:.0}",
                            l.size.width,
                            l.size.height,
                            node.scrollable_overflow().y1
                        );
                    }
                }
            }
        }
    }

    fn window_event(&mut self, event_loop: &dyn ActiveEventLoop, _id: winit::window::WindowId, event: WindowEvent) {
        if matches!(event, WindowEvent::CloseRequested) {
            self.view = None;
            event_loop.exit();
            return;
        }
        match &event {
            WindowEvent::Ime(Ime::Commit(text)) => {
                self.preedit.clear();
                self.insert_str(text);
                self.render_composer();
                return;
            }
            WindowEvent::Ime(Ime::Preedit(text, _)) => {
                self.preedit = text.clone();
                self.render_composer();
                return;
            }
            WindowEvent::PointerMoved { position, .. } => {
                self.pointer = (position.x, position.y);
                let inside = self.hit_composer(*position);
                if inside != self.hover {
                    self.hover = inside;
                    if let Some(v) = self.view.as_ref() {
                        v.window.set_cursor(Cursor::Icon(if inside { CursorIcon::Text } else { CursorIcon::Default }));
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, y) => -(*y as f64) * 44.0,
                    winit::event::MouseScrollDelta::PixelDelta(p) => -p.y,
                    _ => 0.0,
                };
                if matches!(self.ui.menu, Menu::ModelList | Menu::Effort) {
                    self.scroll_menu_list(dy);
                    return;
                }
                if self.sidebar.expanded && self.sidebar_wheel(dy) {
                    return;
                }
            }
            WindowEvent::PointerButton { state, position, .. } => {
                if *state == ElementState::Pressed {
                    if let Some(id) = self.hit_id(*position) {
                        if self.on_chip_click(&id) { return; }
                    }
                    if self.ui.menu != Menu::None {
                        self.ui.menu = Menu::None;
                        self.update_menu_visibility();
                    }
                    if self.hero.menu != HeroMenu::None {
                        self.set_hero_menu(HeroMenu::None);
                    }
                    let inside = self.hit_composer(*position);
                    let was = self.focused;
                    self.focused = inside;
                    if inside {
                        let idx = self.caret_from_click(*position);
                        self.caret = idx;
                    }
                    if inside != was || inside {
                        self.preedit.clear();
                        self.render_composer();
                    }
                }
            }
            WindowEvent::KeyboardInput { event: key, .. } => {
                if key.state == ElementState::Pressed && self.focused {
                    match &key.logical_key {
                        Key::Named(NamedKey::Enter) => { self.send_input(); return; }
                        Key::Named(NamedKey::Backspace) => { self.backspace(); self.render_composer(); return; }
                        Key::Named(NamedKey::Delete) => { self.delete_forward(); self.render_composer(); return; }
                        Key::Named(NamedKey::ArrowLeft) => { self.move_caret_left(); self.render_composer(); return; }
                        Key::Named(NamedKey::ArrowRight) => { self.move_caret_right(); self.render_composer(); return; }
                        Key::Named(NamedKey::Home) => { self.caret = 0; self.render_composer(); return; }
                        Key::Named(NamedKey::End) => { self.caret = self.input.len(); self.render_composer(); return; }
                        Key::Named(NamedKey::Escape) => {
                            if self.ui.menu != Menu::None { self.ui.menu = Menu::None; self.update_menu_visibility(); return; }
                            self.preedit.clear();
                            self.render_composer();
                            return;
                        }
                        _ => {}
                    }
                    if let Some(t) = &key.text {
                        if !t.chars().any(|c| c.is_control()) {
                            self.insert_str(t);
                            self.render_composer();
                            return;
                        }
                    }
                }
            }
            _ => {}
        }
        if let Some(view) = self.view.as_mut() {
            view.handle_winit_event(event);
            self.proxy.send_event(blitz_shell::BlitzShellEvent::Poll { window_id: view.window_id() });
        }
    }

    fn proxy_wake_up(&mut self, event_loop: &dyn ActiveEventLoop) {
        self.about_to_wait(event_loop);
    }

    fn about_to_wait(&mut self, event_loop: &dyn ActiveEventLoop) {
        while let Ok(ev) = self.events.try_recv() {
            use blitz_shell::BlitzShellEvent as E;
            match ev {
                E::Poll { window_id } => {
                    if let Some(view) = self.view.as_mut() {
                        if view.window_id() == window_id { view.poll(); }
                    }
                }
                E::ResumeReady { .. } => {
                    if let Some(view) = self.view.as_mut() { view.complete_resume(); }
                }
                E::RequestRedraw { .. } => {
                    if let Some(view) = self.view.as_mut() { view.request_redraw(); }
                }
                E::CloseWindow { .. } => {
                    self.view = None;
                    event_loop.exit();
                }
                _ => {}
            }
        }
        if !self.clicktest.is_empty() && std::time::Instant::now() >= self.clickat {
            let id = self.clicktest.remove(0);
            let resolved = self.resolve_at_center(&id);
            eprintln!("clicktest: {id} -> hit={resolved:?}");
            self.on_chip_click(&id);
            self.clickat = std::time::Instant::now() + Duration::from_millis(900);
        }
        while let Ok(ev) = self.nav_rx.try_recv() {
            match ev {
                NavEvent::HeroSession(id) => {
                    self.hero.session = Some(id.clone());
                    self.switch_session_into_hero(id);
                }
                NavEvent::Sidebar(sb) => {
                    let mut next = *sb;
                    next.expanded = self.sidebar.expanded;
                    next.collapsed = self.sidebar.collapsed.clone();
                    next.show_all = self.sidebar.show_all.clone();
                    next.current = self.cfg.session.clone();
                    // The refocus that follows a click must not rebuild a page
                    // whose sidebar markup did not actually change.
                    let changed = next.html() != self.sidebar.html();
                    self.sidebar = next;
                    if changed {
                        let items = self.last_items.clone();
                        self.apply(&items);
                    }
                }
            }
        }
        let mut stats_now = None;
        while let Ok(s) = self.stats_rx.try_recv() { stats_now = Some(s); }
        if let Some(s) = stats_now {
            if s != self.stats {
                self.stats = s;
                self.render_stats();
            }
        }
        let mut ui_changed = false;
        while let Ok(ev) = self.ui_rx.try_recv() {
            match ev {
                UiEvent::Permission(value) => {
                    if self.ui.perm_current != value { self.ui.perm_current = value; ui_changed = true; }
                }
                UiEvent::Sel(sel) => {
                    if self.ui.sel.as_ref() != Some(&sel) { self.ui.sel = Some(sel); ui_changed = true; }
                }
            }
        }
        if ui_changed { self.repaint(); }
        let mut latest = None;
        while let Ok(items) = self.updates.try_recv() { latest = Some(items); }
        // The hero shows an empty task; the previous Session keeps streaming.
        if self.hero.active { latest = None; }
        if let Some((items, text, think)) = latest {
            self.pending = Some(items);
            self.render_live(&text, &think);
        }
        // One rebuild per 100 ms at most: a rebuild reparses the whole page, so
        // coalescing stream chunks is what keeps the window responsive.
        if let Some(items) = self.pending.clone() {
            if self.last_apply.elapsed() >= Duration::from_millis(100) && items != self.last_items {
                self.pending = None;
                self.last_apply = std::time::Instant::now();
                self.apply(&items);
            }
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(std::time::Instant::now() + Duration::from_millis(200)));
    }
}


#[derive(Default, Clone)]
struct Block { kind: String, text: String, name: String }

#[derive(Default, Clone)]
struct Live { blocks: Vec<Block> }

impl Live {
    fn apply(&mut self, chunk: &Value) {
        let kind = chunk["type"].as_str().unwrap_or("");
        let idx = chunk["index"].as_u64().unwrap_or(0) as usize;
        match kind {
            "block-start" => {
                let bt = chunk["blockType"].as_str().unwrap_or("text").to_string();
                while self.blocks.len() <= idx { self.blocks.push(Block::default()); }
                self.blocks[idx] = Block { kind: bt, text: String::new(), name: String::new() };
            }
            "text-delta" | "reasoning-delta" | "thinking-delta" => {
                if let Some(t) = chunk["text"].as_str() {
                    while self.blocks.len() <= idx { self.blocks.push(Block::default()); }
                    if self.blocks[idx].kind.is_empty() {
                        self.blocks[idx].kind = if kind == "text-delta" { "text".to_string() } else { "reasoning".to_string() };
                    }
                    self.blocks[idx].text.push_str(t);
                }
            }
            "block-end" => {
                let block = &chunk["block"];
                let bt = block["type"].as_str().unwrap_or("text").to_string();
                while self.blocks.len() <= idx { self.blocks.push(Block::default()); }
                self.blocks[idx].kind = bt.clone();
                if let Some(t) = block["text"].as_str() { self.blocks[idx].text = t.to_string(); }
                if let Some(n) = block["name"].as_str() { self.blocks[idx].name = n.to_string(); }
                if let Some(n) = block["toolName"].as_str() { self.blocks[idx].name = n.to_string(); }
            }
            _ => {}
        }
    }

    fn text(&self) -> String {
        let mut out = String::new();
        for b in &self.blocks {
            if b.kind == "text" || b.kind.is_empty() { out.push_str(&b.text); }
        }
        out
    }

    fn think(&self) -> String {
        let mut out = String::new();
        for b in &self.blocks {
            if b.kind != "reasoning" { continue; }
            if let Some(line) = b.text.lines().map(str::trim).filter(|l| !l.is_empty()).last() {
                if !out.is_empty() { out.push(' '); }
                out.push_str(line);
            }
        }
        out
    }
}

fn push_record(items: &mut Vec<String>, e: &Value, pending: &mut std::collections::HashMap<String, (String, String)>) {
    match e["type"].as_str().unwrap_or("") {
        "user/message" => {
            let kind = e["data"]["source"]["kind"].as_str().unwrap_or("user");
            if kind == "user" { items.push(user_item(&text_of(&e["data"]))); }
        }
        "assistant/message" => {
            let parts = e["data"]["message"]["content"].as_array().cloned().unwrap_or_default();
            let html = assistant_item(&parts);
            if !html.is_empty() { items.push(html); }
        }
        "tool/call" => {
            let name = e["data"]["name"].as_str().unwrap_or("tool").to_string();
            let args_s = e["data"]["arguments"].as_str().unwrap_or("");
            let summary = summary_of(&name, args_s);
            let call_id = e["data"]["callId"].as_str().unwrap_or("").to_string();
            pending.insert(call_id, (name, summary));
        }
        "tool/result" => {
            let call_id = e["data"]["message"]["toolCallId"].as_str().unwrap_or("").to_string();
            let ok = e["data"].get("error").is_none();
            let (name, summary) = pending.remove(&call_id).unwrap_or(("tool".into(), String::new()));
            items.push(tool_item(&name, &summary, ok));
            let images = tool_image_block(&tool_images(&e["data"]));
            if !images.is_empty() {
                items.push(images);
            }
        }
        _ => {}
    }
}

async fn stream_session(
    cfg: Cfg,
    tx: std::sync::mpsc::Sender<(String, String, String)>,
    switch: &mut tokio::sync::watch::Receiver<String>,
) -> Result<()> {
    let mux = format!("{}/api/remote.mux", cfg.origin.replacen("http", "ws", 1));
    let mut req = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(mux.as_str())?;
    {
        use tokio_tungstenite::tungstenite::http::HeaderValue;
        let h = req.headers_mut();
        h.insert("Origin", HeaderValue::from_str(&cfg.origin)?);
        h.insert("Cookie", HeaderValue::from_str(&cfg.cookie)?);
    }
    let (ws, _) = tokio_tungstenite::connect_async(req).await?;
    let (mut sock_tx, mut sock_rx) = ws.split();

    let args = serde_json::json!({ "request": {
        "address": { "kind": "session", "sessionId": cfg.session },
        "activate": true,
        "assistantStream": true
    }});
    sock_tx.send(Message::Text(open_frame("f", "session/follow", args).into())).await?;

    let mut items: Vec<String> = Vec::new();
    let mut pending: std::collections::HashMap<String, (String, String)> = Default::default();
    let mut live = Live::default();
    let mut dirty = true;
    let mut last_send = std::time::Instant::now() - Duration::from_secs(1);

    loop {
        let frame = tokio::select! {
            changed = switch.changed() => {
                let moved = changed.is_ok() && *switch.borrow() != cfg.session;
                if moved { return Ok(()); }
                continue;
            }
            frame = sock_rx.next() => frame,
        };
        let Some(frame) = frame else { break };
        let Ok(msg) = frame else { break };
        let Message::Text(t) = msg else { continue };
        let v: Value = match serde_json::from_str(&t.to_string()) { Ok(v) => v, Err(_) => continue };
        if v["streamId"] != "f" { continue; }
        let val = &v["value"];
        match val["type"].as_str().unwrap_or("") {
            "snapshot" => {
                let all = val["records"].as_array().cloned().unwrap_or_default();
                let skip = all.len().saturating_sub(200);
                items.clear();
                for rec in all.into_iter().skip(skip) {
                    push_record(&mut items, &rec["event"], &mut pending);
                }
                dirty = true;
            }
            "event" => {
                push_record(&mut items, &val["event"], &mut pending);
                live = Live::default();
                dirty = true;
            }
            "assistant-stream" => {
                let f = &val["frame"];
                match f["type"].as_str().unwrap_or("") {
                    "start" => { live = Live::default(); }
                    "chunk" => { live.apply(&f["chunk"]); }
                    _ => {}
                }
                dirty = true;
            }
            _ => {}
        }
        if dirty && last_send.elapsed() >= Duration::from_millis(50) {
            let _ = tx.send((items.concat(), live.text(), live.think()));
            dirty = false;
            last_send = std::time::Instant::now();
        }
    }
    Ok(())
}


fn main() -> Result<()> {
    limit_malloc_arenas();
    let argv: Vec<String> = std::env::args().collect();
    let (cookie, origin) = if let Some(i) = argv.iter().position(|a| a == "--url") {
        let rt = rt_current()?;
        rt.block_on(pair_url(argv.get(i + 1).ok_or_else(|| anyhow!("--url needs a value"))?))?
    } else { (load_cookie()?, load_origin()) };

    let rt = rt_current()?;
    let sid = match argv.iter().position(|a| a == "--session").and_then(|i| argv.get(i + 1).cloned()) {
        Some(s) => s,
        None => rt.block_on(pick_session(&cookie, &origin))?,
    };
    let cfg = Cfg { cookie, origin, session: sid.clone() };
    let (seq, items) = rt.block_on(fetch_items(&cfg))?;
    let ui = rt.block_on(load_ui(&cfg));
    let sidebar = rt.block_on(load_sidebar(&cfg));
    let hero = Hero { presets: rt.block_on(load_presets(&cfg)), preset: None, ..Default::default() };
    if let Ok(dump) = std::env::var("DSH_HERO") {
        let mut h = hero.clone();
        h.active = true;
        h.workspace = sidebar.workspaces.first().map(|w| w.id.clone());
        h.preset = Some("ptc".to_string());
        h.menu = match dump.as_str() {
            "ws" => HeroMenu::Workspace,
            "preset" => HeroMenu::Preset,
            _ => HeroMenu::None,
        };
        let dump = dump.as_str();
        let mut ui3 = ui.clone();
        if dump == "perm" {
            ui3.menu = Menu::Permission;
        }
        let page = build_page("", "New Session", &ui3, &sidebar, &h);
        std::fs::write(out_page(), &page)?;
        eprintln!("dumped hero");
        return Ok(());
    }
    if std::env::var("DSH_MDTEST").is_ok() {
        let sample = "# Heading one\n\nIntro with **bold**, *italic*, `code` and a [link](https://example.com).\n\n![shot](/home/neutron42/dev/dsh-blitz/capture/sb5_crop.png)\n\n## List\n\n- first item\n- second item\n  - nested item\n  - nested two\n- third\n\n1. one\n2. two\n3. three\n\n| col a | col b |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |\n\n> quoted line\n\n```rust\nfn main() { println!(\"hi\"); }\n```\n\n---\n\nFinal paragraph.\n";
        let parts = vec![serde_json::json!({ "type": "text", "text": sample })];
        let items = assistant_item(&parts);
        let page = build_page(&items, "Markdown test", &ui, &sidebar, &hero);
        std::fs::write(out_page(), &page)?;
        eprintln!("dumped markdown sample");
        return Ok(());
    }
    if let Ok(dump) = std::env::var("DSH_DUMP") {
        let menu = match dump.as_str() {
            "permission" => Menu::Permission,
            "model" => Menu::Model,
            "list" => Menu::ModelList,
            "effort" => Menu::Effort,
            _ => Menu::None,
        };
        let mut ui2 = ui.clone();
        ui2.menu = menu;
        let mut sb2 = sidebar.clone();
        sb2.expanded = dump == "sidebar";
        let title = sb2.title_of(&sid);
        let page = build_page(&items, &title, &ui2, &sb2, &hero);
        std::fs::write(out_page(), &page)?;
        eprintln!("dumped {} with menu {dump}", out_page());
        return Ok(());
    }
    let title = sidebar.title_of(&sid);
    let page = build_page(&items, &title, &ui, &sidebar, &hero);
    std::fs::write(out_page(), &page)?;
    eprintln!(
        "ready: {seq} seq, {} bytes, {} workspaces, {} sessions",
        page.len(),
        sidebar.workspaces.len(),
        sidebar.sessions.len()
    );

    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiEvent>();
    let (tx, rx) = std::sync::mpsc::channel::<(String, String, String)>();
    let (session_tx, session_rx) = tokio::sync::watch::channel(sid.clone());
    let cfg2 = cfg.clone();
    std::thread::spawn(move || {
        let rt = rt_current().unwrap();
        let mut switch = session_rx;
        loop {
            let mut next = cfg2.clone();
            next.session = switch.borrow().clone();
            let _ = rt.block_on(stream_session(next, tx.clone(), &mut switch));
            std::thread::sleep(Duration::from_millis(300));
        }
    });
    let (nav_tx, nav_rx) = std::sync::mpsc::channel::<NavEvent>();
    let (stats_tx, stats_rx) = std::sync::mpsc::channel::<Stats>();
    {
        let cfg3 = cfg.clone();
        let watch = session_tx.subscribe();
        std::thread::spawn(move || {
            let rt = rt_current().unwrap();
            let mut last = Stats::default();
            loop {
                let sid = watch.borrow().clone();
                if let Some(s) = rt.block_on(fetch_stats(&cfg3, &sid)) {
                    if s != last {
                        last = s.clone();
                        let _ = stats_tx.send(s);
                    }
                }
                for _ in 0..12 {
                    std::thread::sleep(Duration::from_millis(100));
                    if watch.has_changed().unwrap_or(false) {
                        break;
                    }
                }
            }
        });
    }

    let event_loop = create_default_event_loop();
    let (proxy, events) = BlitzShellProxy::new(event_loop.create_proxy());
    let shell = Shell {
        title: title.clone(),
        cfg: cfg.clone(),
        input: String::new(),
        caret: 0,
        preedit: String::new(),
        focused: false,
        hover: false,
        caret_node: None,
        proxy,
        events,
        view: None,
        updates: rx,
        page,
        last_seq: seq,
        last_items: items,
        ui,
        ui_tx,
        ui_rx,
        clicktest: std::env::var("DSH_CLICKTEST").ok()
            .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
            .unwrap_or_default(),
        clickat: std::time::Instant::now() + Duration::from_millis(1500),
        hero,
        font_ctx: parley::FontContext::new(),
        pending: None,
        last_apply: std::time::Instant::now(),
        chrome: String::new(),
        stats: Stats::default(),
        stats_rx,
        sidebar,
        nav_tx,
        nav_rx,
        session_tx,
        pointer: (0.0, 0.0),
    };
    event_loop.run_app(shell).unwrap();
    Ok(())
}
