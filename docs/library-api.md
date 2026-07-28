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

Publish from one task at a time — clones share the channel so the handle can be
passed around, not so several tasks can race a `publish` against a
`switch_source`.

### The link table is append-only within an epoch

Deltas carry only OSC 8 link ids the viewer hasn't seen yet, so re-using an id
for a different URI inside one epoch leaves viewers resolving it to the old
target. Either keep ids unique for the life of the epoch, or call
`switch_source` — the resulting full frame republishes the whole table. (The
built-in PTY producer gets this free from vt100's monotonic ids.)

## Presentation and transport

```rust
let presentation = shellglass::api::Presentation::load(config_path)?;
let options = shellglass::api::ServeOptions::new("127.0.0.1:8080");
shellglass::api::serve(|| Ok(source), presentation, options).await?;
```

`ServeOptions` and `PushOptions` are `#[non_exhaustive]` — build them with
`::new()` and assign the fields you need, so a new option is not a breaking
change. To stop the server on your own signal, use `serve_with_shutdown`, which
takes a future and shuts down gracefully when it resolves. It closes long-lived
SSE and SSH viewers, stops source-forwarding tasks, and flushes an active
recording before returning, so the same runtime can start another server.

For a hub, `api::push` takes the same source factory and a `PushOptions`. The
factory is invoked only after the authenticated WebSocket upgrade succeeds.

Library-only Cargo features avoid compiling the built-in PTY producer:

```toml
shellglass = { path = "../shellglass", default-features = false, features = ["serve-api", "push-api"] }
```

The stock `serve` and `push` features retain the PTY-backed CLI unchanged. A
runnable synthetic producer is in [`examples/external-source.rs`](../examples/external-source.rs).
