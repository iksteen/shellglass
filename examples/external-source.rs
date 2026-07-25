//! Minimal parser-independent producer using shellglass's stock HTTP/SSE viewer.

use shellglass::api::{Presentation, ServeOptions, external_source, serve};
use shellglass::model::{Color, Frame, Grid, StyledCell};
use std::time::Duration;

fn frame(counter: u64) -> Frame {
    let text = format!("external frame {counter}");
    Frame::Screen(Grid {
        source_epoch: 0,
        cols: text.chars().count() as u16,
        rows: vec![
            text.chars()
                .map(|ch| StyledCell {
                    text: ch.to_string(),
                    ..Default::default()
                })
                .collect(),
        ],
        cursor: None,
        cursor_style: 0,
        default_colors: (Color::Default, Color::Default),
        title: "synthetic external source".into(),
        links: Default::default(),
        images: Vec::new(),
        image_data: Default::default(),
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (publisher, source) = external_source(frame(0));
    tokio::spawn(async move {
        for counter in 1.. {
            tokio::time::sleep(Duration::from_secs(1)).await;
            publisher.publish(frame(counter));
        }
    });

    serve(
        || Ok(source),
        Presentation::load(None)?,
        ServeOptions::new("127.0.0.1:8080"),
    )
    .await
}
