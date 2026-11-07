use blitz_dom::{DocumentConfig, local_name};
use blitz_html::HtmlDocument;
use blitz_net::Provider;
use blitz_traits::shell::{ColorScheme, Viewport};
use std::sync::Arc;

fn box_of(doc: &mut HtmlDocument, id: &str) {
    let Some(n) = doc.get_element_by_id(id) else { println!("{id}: MISSING"); return };
    let Some(node) = doc.get_node(n) else { return };
    let l = node.final_layout();
    println!("{id}: x={:.1} y={:.1} w={:.1} h={:.1}", l.location.x, l.location.y, l.size.width, l.size.height);
    let kids = doc.mutate().child_ids(n);
    for k in kids {
        if let Some(kn) = doc.get_node(k) {
            if kn.is_element() {
                let kl = kn.final_layout();
                let tag = kn.element_data().map(|e| e.name.local.to_string()).unwrap_or_default();
                let cls = kn.attr(local_name!("class")).unwrap_or("").to_string();
                println!("   child <{tag} class=\"{}\"> x={:.1} w={:.1} h={:.1}", cls.chars().take(60).collect::<String>(), kl.location.x, kl.size.width, kl.size.height);
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let path = std::env::args().nth(1).expect("html path");
    let html = std::fs::read_to_string(&path).expect("read html");
    let net = Arc::new(Provider::new(None));
    let mut doc = HtmlDocument::from_html(&html, DocumentConfig {
        base_url: Some(format!("file://{path}")),
        net_provider: Some(Arc::clone(&net) as _),
        viewport: Some(Viewport::new(1440, 900, 1.0, ColorScheme::Light)),
        ..Default::default()
    });
    doc.resolve(0.0);
    doc.resolve(0.0);
    box_of(&mut doc, "composer-card");
    box_of(&mut doc, "menu-layer");
    for id in ["menu-permission", "menu-model", "menu-model-list", "menu-effort"] {
        box_of(&mut doc, id);
    }
    box_of(&mut doc, "model-cell-effort");
}
