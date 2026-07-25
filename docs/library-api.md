# External frame-producer API

Shellglass accepts parser-independent terminal producers without knowing how they
capture a screen. Hooking, accessibility, replay, and remote-agent projects own
their acquisition and policy; shellglass owns presentation and transport.

```text
external producer -> FramePublisher / SourceSession
                  -> diff + viewer + serve/push/recording/SSH
```

## Producer boundary

```rust
use shellglass::api::external_source;

let (publisher, source) = external_source(initial_frame);
publisher.publish(next_frame);       // newest-wins replacement
publisher.switch_source(first_frame); // increments source_epoch; forces full
```

`source_epoch` is producer-only and never enters the viewer wire. A change resets
same-sized source transitions with a full frame, including links, cursor, images,
and defaults.

## Presentation and transport

```rust
let presentation = shellglass::api::Presentation::load(config_path)?;
let options = shellglass::api::ServeOptions::new("127.0.0.1:8080");
shellglass::api::serve(|| Ok(source), presentation, options).await?;
```

For a hub, `api::push` takes the same source factory and a `PushOptions`. The
factory is invoked only after the authenticated WebSocket upgrade succeeds.

Library-only Cargo features avoid compiling the built-in PTY producer:

```toml
shellglass = { path = "../shellglass", default-features = false, features = ["serve-api", "push-api"] }
```

The stock `serve` and `push` features retain the PTY-backed CLI unchanged. A
runnable synthetic producer is in [`examples/external-source.rs`](../examples/external-source.rs).
