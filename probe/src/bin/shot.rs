use anyrender::{render_to_buffer, PaintScene as _};
use anyrender_vello::VelloImageRenderer;
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::{util::Color, DocumentConfig};
use blitz_html::HtmlDocument;
use blitz_net::Provider;
use blitz_paint::paint_scene;
use blitz_traits::shell::{ColorScheme, Viewport};
use peniko::{kurbo::Rect, Fill};
use std::{fs::File, io::Write, sync::Arc};

#[tokio::main]
async fn main() {
    let path = std::env::args().nth(1).expect("html path");
    let width: u32 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(1440);
    let height: u32 = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(900);
    let out = std::env::args().nth(4).unwrap_or("out.png".to_string());
    let scale: f32 = std::env::var("SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or(1.0);
    let gpu = std::env::var("GPU").is_ok();

    let html = std::fs::read_to_string(&path).expect("read html");
    let url_string = format!("file://{path}");
    let net = Arc::new(Provider::new(None));
    let mut document = HtmlDocument::from_html(
        &html,
        DocumentConfig {
            base_url: Some(url_string),
            net_provider: Some(Arc::clone(&net) as _),
            viewport: if std::env::var("NOVP").is_ok() { None } else { Some(Viewport::new((width as f32 * scale) as u32, (height as f32 * scale) as u32, scale, ColorScheme::Light)) },
            ..Default::default()
        },
    );
    loop {
        document.resolve(0.0);
        if net.is_empty() {
            break;
        }
    }
    document.as_mut().resolve(0.0);

    let computed_height = document.as_ref().root_element().final_layout().size.height;
    let render_width = (width as f32 * scale) as u32;
    let render_height = (((computed_height as f64).max(height as f64) * scale as f64).min(6000.0)) as u32;

    let buffer = if gpu {
        render_to_buffer::<VelloImageRenderer, _>(
            |scene| {
                scene.fill(Fill::NonZero, Default::default(), Color::WHITE, Default::default(), &Rect::new(0.0, 0.0, render_width as f64, render_height as f64));
                paint_scene(scene, document.as_mut(), scale as f64, render_width, render_height, 0, 0);
            },
            render_width,
            render_height,
        )
    } else {
        render_to_buffer::<VelloCpuImageRenderer, _>(
            |scene| {
                scene.fill(Fill::NonZero, Default::default(), Color::WHITE, Default::default(), &Rect::new(0.0, 0.0, render_width as f64, render_height as f64));
                paint_scene(scene, document.as_mut(), scale as f64, render_width, render_height, 0, 0);
            },
            render_width,
            render_height,
        )
    };

    let mut file = File::create(&out).unwrap();
    let mut encoder = png::Encoder::new(&mut file, render_width, render_height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut w = encoder.write_header().unwrap();
    w.write_image_data(&buffer).unwrap();
    w.finish().unwrap();
    file.flush().ok();
    println!("wrote {out} ({render_width}x{render_height}) gpu={gpu}");
}