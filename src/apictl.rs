//! Hub-control client: drive the hub's session-management API from the CLI
//! (`shellglass sessions …`, or the `shellglass-sessions` binary).
//!
//! A thin HTTP client over the `/api/sessions` routes with the same explicit
//! delete semantics as the API itself: `remove --id` and `remove --slug` name
//! the namespace — there is no guessing form, because an un-aliased slug IS
//! the session id. Authenticates with `Authorization: Bearer <key>` in the
//! API salt domain (the key's `api_id` must be on the hub's `--api-allow`).

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .build()
        .context("building HTTP client")
}

fn base(hub: &str) -> String {
    format!("{}/api/sessions", hub.trim_end_matches('/'))
}

/// Make a hub-supplied string safe to print to the operator's terminal. The hub
/// (or a MITM) is untrusted here: an error body or a session id/slug could embed
/// terminal control sequences — including via JSON unicode escapes — and inject
/// into the operator's terminal. Unlike a transport error (whose *kind* is the
/// signal, so we drop the text), these strings ARE the content the operator wants,
/// so we neuter rather than discard: strip control characters and bound the length
/// so a giant body can't flood the screen.
fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(256).collect()
}

/// Read a hub response body into a String, capped so a hostile/MITM hub can't OOM
/// the CLI with an unbounded (or lying-`Content-Length`) body — `Response::text`
/// buffers the whole thing with no ceiling. API responses are tiny; the cap is
/// generous for any real session list.
async fn body_capped(mut res: reqwest::Response) -> Result<String> {
    const MAX: usize = 8 << 20;
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = res.chunk().await.context("reading the hub response")? {
        if buf.len() + chunk.len() > MAX {
            bail!("hub response body exceeds {} MiB — refusing", MAX >> 20);
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).context("hub response body is not valid UTF-8")
}

/// Read the (capped) response body, returning it on success. On a non-success
/// status, turn the body into a readable error instead — the API's own
/// `{"error": …}` message when present, the raw body otherwise.
async fn check(res: reqwest::Response) -> Result<String> {
    let status = res.status();
    if status.is_success() {
        return body_capped(res).await;
    }
    // Tolerant on the error path: a body we can't read just yields the status line.
    let body = body_capped(res).await.unwrap_or_default();
    // The hub is untrusted: neuter its message before it can reach the terminal.
    let msg = clean(
        &serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or(body),
    );
    match status {
        StatusCode::NOT_FOUND if msg.is_empty() => {
            bail!("{status}: not found — is the hub's management API enabled (--api-allow)?")
        }
        StatusCode::UNAUTHORIZED => bail!("{status}: missing/unusable API key"),
        StatusCode::FORBIDDEN => {
            bail!("{status}: key not authorized — is its API id on the hub's --api-allow?")
        }
        _ if msg.is_empty() => bail!("{status}"),
        _ => bail!("{status}: {msg}"),
    }
}

/// `GET /api/sessions` — print every registered session.
pub async fn list(hub: &str, key: &str) -> Result<()> {
    let res = client()?
        .get(base(hub))
        .bearer_auth(key)
        .send()
        .await
        .context("requesting the session list")?;
    let body = check(res).await?;
    let sessions: Vec<serde_json::Value> =
        serde_json::from_str(&body).context("parsing the session list")?;
    if sessions.is_empty() {
        println!("no sessions registered");
        return Ok(());
    }
    println!("{:<24} {:<8} {:<16} SESSION ID", "SLUG", "STATE", "VIEWERS");
    for s in sessions {
        // Render every `<name>Viewers` count the hub reports (e.g. `web 2 ssh 1`),
        // so a new viewer transport appears here without a CLI change. Names are
        // `clean`ed — the hub is untrusted (see module docs).
        let mut viewers: Vec<String> = s
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| {
                Some(format!(
                    "{} {}",
                    clean(k.strip_suffix("Viewers")?),
                    v.as_u64()?
                ))
            })
            .collect();
        viewers.sort();
        let viewers = if viewers.is_empty() {
            "-".to_string()
        } else {
            viewers.join(" ")
        };
        println!(
            "{:<24} {:<8} {:<16} {}",
            clean(s["slug"].as_str().unwrap_or("?")),
            if s["live"].as_bool().unwrap_or(false) {
                "live"
            } else {
                "offline"
            },
            viewers,
            clean(s["id"].as_str().unwrap_or("?")),
        );
    }
    Ok(())
}

/// `POST /api/sessions` — register a session by its public id.
pub async fn add(hub: &str, key: &str, id: &str, slug: Option<&str>) -> Result<()> {
    let mut body = serde_json::json!({ "id": id });
    if let Some(slug) = slug {
        body["slug"] = slug.into();
    }
    let res = client()?
        .post(base(hub))
        .bearer_auth(key)
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .context("adding the session")?;
    let created: serde_json::Value =
        serde_json::from_str(&check(res).await?).context("parsing the add response")?;
    println!(
        "added {} — view at {}/s/{}",
        clean(created["id"].as_str().unwrap_or(id)),
        hub.trim_end_matches('/'),
        clean(created["slug"].as_str().unwrap_or(id)),
    );
    Ok(())
}

/// `DELETE /api/sessions/by-id/{id}` — remove BY SESSION ID.
pub async fn remove_by_id(hub: &str, key: &str, id: &str) -> Result<()> {
    let res = client()?
        .delete(format!("{}/by-id/{id}", base(hub)))
        .bearer_auth(key)
        .send()
        .await
        .context("removing the session")?;
    check(res).await?;
    println!("removed session {id}");
    Ok(())
}

/// `DELETE /api/sessions/by-slug/{slug}` — remove BY VIEW SLUG.
pub async fn remove_by_slug(hub: &str, key: &str, slug: &str) -> Result<()> {
    let res = client()?
        .delete(format!("{}/by-slug/{slug}", base(hub)))
        .bearer_auth(key)
        .send()
        .await
        .context("removing the session")?;
    check(res).await?;
    println!("removed session with slug {slug}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::clean;

    #[test]
    fn clean_neuters_and_bounds_hub_text() {
        assert_eq!(clean("slug-ok"), "slug-ok", "ordinary text is untouched");
        // Control characters (here a CSI clear-screen) are stripped — no escape
        // sequence can survive to reach the operator's terminal.
        let evil = "\x1b[2J\x1b[1;1Hgotcha";
        let out = clean(evil);
        assert!(
            !out.chars().any(char::is_control),
            "no controls survive: {out:?}"
        );
        assert_eq!(out, "[2J[1;1Hgotcha");
        // A giant body can't flood the screen.
        assert!(clean(&"x".repeat(10_000)).chars().count() <= 256);
    }
}
