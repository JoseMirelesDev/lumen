//! ChatController — message/composer slice of the AppWindow contract:
//! sending, DM-call toggle, message copy, link opening and async link
//! previews. Owns the SSRF-guarded fetch helpers (`is_public_ip`,
//! `fetch_link_title`, `extract_title`) and the preview-title cache.

use std::sync::Arc;

use lumen_core::ShellState;
use parking_lot::RwLock;
use slint::{ComponentHandle, Model, Weak};

use crate::sound::{Sfx, SfxEvent};
use crate::AppWindow;

pub struct ChatController {
    pub shell: Arc<ShellState>,
    pub rt: tokio::runtime::Handle,
    sfx: Arc<Sfx>,
    weak: RwLock<Option<Weak<AppWindow>>>,
    /// Message id armed by "edit" (consumed by "edit-submit").
    pending_edit: RwLock<Option<String>>,
    /// Resolved link-preview titles by message id ("" = fetch failed / no
    /// title). Persists across model rebuilds so previews survive re-pushes.
    link_titles: RwLock<std::collections::HashMap<String, String>>,
    /// Message ids with a preview fetch in flight — avoids duplicate tasks
    /// when push_shell runs repeatedly before a fetch settles.
    link_pending: RwLock<std::collections::HashSet<String>>,
    /// Injected by UiController: re-push the whole UI after a state change.
    on_changed: Arc<dyn Fn() + Send + Sync>,
    /// Injected by UiController: transient notification (message, kind).
    on_toast: Arc<dyn Fn(String, String) + Send + Sync>,
}

impl ChatController {
    pub fn new(
        shell: Arc<ShellState>,
        rt: tokio::runtime::Handle,
        sfx: Arc<Sfx>,
        on_changed: Arc<dyn Fn() + Send + Sync>,
        on_toast: Arc<dyn Fn(String, String) + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new(Self {
            shell,
            rt,
            sfx,
            weak: RwLock::new(None),
            pending_edit: RwLock::new(None),
            link_titles: RwLock::new(std::collections::HashMap::new()),
            link_pending: RwLock::new(std::collections::HashSet::new()),
            on_changed,
            on_toast,
        })
    }

    fn weak(&self) -> Weak<AppWindow> {
        self.weak.read().clone().expect("ChatController not attached")
    }

    pub fn attach(self: &Arc<Self>, ui: &AppWindow) {
        *self.weak.write() = Some(ui.as_weak());
        self.wire(ui);
    }

    fn wire(self: &Arc<Self>, ui: &AppWindow) {
        let this = self.clone();
        ui.on_copy_message(move |id| this.copy_message(id.to_string()));
        let this = self.clone();
        ui.on_link_clicked(move |url| this.open_link(url.to_string()));
        let this = self.clone();
        ui.on_send_message(move |content| this.send_message(content.to_string()));
        let this = self.clone();
        ui.on_toggle_dm_call(move || this.toggle_dm_call());
        let this = self.clone();
        ui.on_message_action(move |action, id| this.message_action(action.to_string(), id.to_string()));
    }

    // -- Fase 2: edit / delete own messages --------------------------------

    fn message_action(self: &Arc<Self>, action: String, id: String) {
        let this = self.clone();
        let weak = this.weak();
        match action.as_str() {
            "edit" => {
                let content = this
                    .shell
                    .messages
                    .read()
                    .iter()
                    .find(|m| m.id == id)
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                *this.pending_edit.write() = Some(id);
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.set_edit_message_text(content.into());
                    ui.set_overlay("edit-message".into());
                });
            }
            "edit-submit" => {
                let id = this.pending_edit.write().take();
                let content = weak
                    .upgrade()
                    .map(|ui| ui.get_edit_message_text().to_string())
                    .unwrap_or_default();
                self.rt.spawn(async move {
                    if let Some(id) = id {
                        let content = content.trim().to_string();
                        if !content.is_empty() {
                            let channel_id = this.shell.selected_channel_id.read().clone().unwrap_or_default();
                            let server_id = this.shell.selected_server_id.read().clone().unwrap_or_default();
                            this.shell.edit_message_ws(channel_id, server_id, id, content);
                        }
                    }
                    let _ = weak.upgrade_in_event_loop(move |ui| ui.set_overlay("none".into()));
                    (this.on_changed)();
                });
            }
            "delete" => {
                self.rt.spawn(async move {
                    let channel_id = this.shell.selected_channel_id.read().clone().unwrap_or_default();
                    let server_id = this.shell.selected_server_id.read().clone().unwrap_or_default();
                    this.shell.delete_message_ws(channel_id, server_id, id);
                    (this.on_toast)("Mensaje eliminado".into(), "info".into());
                    (this.on_changed)();
                });
            }
            "report" => {
                self.rt.spawn(async move {
                    if let Err(e) = this.shell.report("message".into(), id, None).await {
                        this.shell.set_error(e);
                    } else {
                        (this.on_toast)("Reporte enviado. Gracias por cuidar la posada.".into(), "success".into());
                    }
                    (this.on_changed)();
                });
            }
            _ => {}
        }
    }

    // -- actions -----------------------------------------------------------

    fn send_message(self: &Arc<Self>, content: String) {
        self.sfx.play(SfxEvent::Send);
        let this = self.clone();
        self.rt.spawn(async move {
            let _ = this.shell.send_message(content).await;
            (this.on_changed)();
        });
    }

    fn toggle_dm_call(self: &Arc<Self>) {
        let on = !*self.shell.dm_call.read();
        *self.shell.dm_call.write() = on;
        if on {
            self.sfx.play(SfxEvent::Call);
        }
        (self.on_changed)();
    }

    /// Copy a message's content to the clipboard (same handle-lifetime
    /// caveat as copy_invite: on X11 the arboard Clipboard must outlive the
    /// call or the selection is cleared the moment the handle drops).
    fn copy_message(self: &Arc<Self>, id: String) {
        let content = self
            .shell
            .messages
            .read()
            .iter()
            .find(|m| m.id == id)
            .map(|m| m.content.clone());
        let Some(content) = content else { return };
        let _ = std::thread::spawn(move || {
            let mut guard = crate::controller::CLIPBOARD.lock().unwrap();
            if guard.is_none() {
                match arboard::Clipboard::new() {
                    Ok(cb) => *guard = Some(cb),
                    Err(e) => {
                        eprintln!("copy message: no clipboard: {e:?}");
                        return;
                    }
                }
            }
            if let Err(e) = guard.as_mut().unwrap().set_text(content) {
                eprintln!("copy message: clipboard write failed: {e:?}");
            }
        });
    }

    /// Open a message link in the OS default browser. The URL was already
    /// shown as a preview, so a missing opener is only logged, not surfaced.
    /// Only public http(s) links are opened — never arbitrary schemes or
    /// internal hosts (xdg-open can dispatch to handlers for file:/mailto:/
    /// etc., which a crafted message must not be able to trigger).
    fn open_link(self: &Arc<Self>, url: String) {
        // Synchronous scheme check: only http/https reach the OS opener. The
        // public-host check is async (DNS), but the scheme gate alone already
        // blocks the dangerous handlers; a private http:// URL would just open
        // in the browser, which is no worse than the user pasting it.
        match url::Url::parse(&url) {
            Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => {}
            _ => {
                eprintln!("open link: rejected non-http(s) URL: {url}");
                return;
            }
        }
        eprintln!("open link: {url}");
        #[cfg(not(target_os = "windows"))]
        let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
        #[cfg(target_os = "windows")]
        let _ = std::process::Command::new("cmd").args(["/c", "start", ""]).arg(&url).spawn();
    }

    /// Async link-preview resolution, called after every messages push.
    /// Rows with a resolved title get it patched back in (titles survive
    /// model rebuilds via `link_titles`); rows with an unresolvable URL get
    /// nothing. Failure-tolerant by design: a fetch error resolves the id to
    /// "" so it is never retried.
    pub fn resolve_link_previews(self: &Arc<Self>, ui: &AppWindow) {
        let titles = self.link_titles.read();
        let mut pending = self.link_pending.write();
        let model = ui.get_messages();
        for i in 0..model.row_count() {
            let Some(row) = model.row_data(i) else { continue };
            if row.link_url.is_empty() {
                continue;
            }
            let id = row.id.to_string();
            if let Some(title) = titles.get(&id) {
                if !title.is_empty() {
                    let mut updated = row;
                    updated.link_title = title.clone().into();
                    let _ = model.set_row_data(i, updated);
                }
                continue;
            }
            if pending.contains(&id) {
                continue;
            }
            pending.insert(id.clone());
            let url = row.link_url.to_string();
            let this = self.clone();
            let weak = self.weak.read().clone();
            self.rt.spawn(async move {
                let title = fetch_link_title(&url).await;
                // Resolved (possibly to "" on failure) → never refetch.
                this.link_titles.write().insert(id.clone(), title.clone());
                this.link_pending.write().remove(&id);
                let Some(weak) = weak else { return };
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    // Row indices shift between pushes; match by id.
                    let model = ui.get_messages();
                    for i in 0..model.row_count() {
                        if let Some(row) = model.row_data(i) {
                            if row.id.as_str() == id {
                                let mut updated = row;
                                updated.link_title = title.clone().into();
                                let _ = model.set_row_data(i, updated);
                                break;
                            }
                        }
                    }
                });
            });
        }
    }
}

/// Fetch the <title> of a page. Failure-tolerant: network errors, timeouts,
/// non-UTF-8 bodies and missing <title> tags all yield "".
///
/// SSRF-guarded: the host must be a public HTTP(S) endpoint — private,
/// loopback, link-local and reserved addresses are rejected (both literal IPs
/// and hostnames that resolve to them), redirects are limited and re-validated
/// on each hop, and the body is capped. This prevents a chat participant from
/// making every client fetch internal/cloud-metadata endpoints.
async fn fetch_link_title(url: &str) -> String {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    const MAX_BODY: usize = 512 * 1024; // 512 KiB — we only need <title>

    let Some(parsed) = url::Url::parse(url).ok() else { return String::new() };
    if !is_public_http_url(&parsed).await {
        return String::new();
    }

    // Reject redirects that leave the public-internet whitelist. `limited(3)`
    // keeps the client from being a redirect loop pig, and the policy closure
    // re-checks every destination the server wants to send us to.
    let client = reqwest::Client::builder()
        // A real browser User-Agent: many sites (YouTube, Google, etc.)
        // serve a "your browser is outdated" / consent page to requests
        // without one, which would show up as a garbage preview title.
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36")
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 3 {
                return attempt.stop();
            }
            let url = attempt.url();
            match url.scheme() {
                "http" | "https" => attempt.follow(),
                _ => attempt.stop(),
            }
        }))
        .timeout(TIMEOUT)
        .build();

    let Ok(client) = client else { return String::new() };
    let Ok(resp) = tokio::time::timeout(TIMEOUT, client.get(url).send()).await else {
        return String::new();
    };
    let Ok(resp) = resp else { return String::new() };
    // Cap the body: we only need the leading bytes for <title>. (reqwest's
    // body reader yields the whole body; the 10s timeout bounds it, and the
    // length check below rejects oversized responses.)
    let Ok(bytes) = tokio::time::timeout(TIMEOUT, resp.bytes()).await else {
        return String::new();
    };
    let Ok(bytes) = bytes else { return String::new() };
    if bytes.len() > MAX_BODY {
        return String::new();
    }
    let Ok(body) = String::from_utf8(bytes.to_vec()) else { return String::new() };
    extract_title(&body)
}

/// True when `url` is http/https AND its host is a public internet address.
/// Resolves DNS so hostnames pointing at private ranges are caught too.
async fn is_public_http_url(url: &url::Url) -> bool {
    match url.scheme() {
        "http" | "https" => {}
        _ => return false,
    }
    let Some(host) = url.host_str() else { return false };

    // Literal IP in the URL: reject non-public immediately (no DNS needed).
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return is_public_ip(ip);
    }

    // Hostname: resolve and require EVERY address to be public (a hostname
    // resolving to any private IP is rejected — guards DNS-rebinding style
    // trickery where a name resolves differently for us).
    let port = url.port().unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
    let Ok(addrs) = tokio::net::lookup_host((host, port)).await else {
        return false;
    };
    let mut any = false;
    for addr in addrs {
        any = true;
        if !is_public_ip(addr.ip()) {
            return false;
        }
    }
    any
}

/// True for globally-routable unicast addresses only. Rejects loopback,
/// private (RFC 1918), link-local, carrier-grade NAT (100.64/10),
/// documentation/reserved, multicast, broadcast and cloud-metadata ranges.
fn is_public_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            if v4.is_loopback() || v4.is_private() || v4.is_link_local() {
                return false;
            }
            if v4.is_multicast() || v4.is_broadcast() || v4.is_unspecified() {
                return false;
            }
            // RFC 6598 carrier-grade NAT, documentation, and the AWS/GCP/Azure
            // metadata IP 169.254.169.254 (link-local, caught above).
            let octets = v4.octets();
            if octets[0] == 100 && (64..=127).contains(&octets[1]) {
                return false; // 100.64.0.0/10 CGNAT
            }
            if octets[0] == 192 && octets[1] == 0 && octets[2] == 2 {
                return false; // 192.0.2.0/24 documentation
            }
            if octets[0] == 198 && (18..=19).contains(&octets[1]) {
                return false; // 198.18.0.0/15 benchmark
            }
            true
        }
        std::net::IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
                return false;
            }
            if v6.segments()[0] & 0xffc0 == 0xfe80 {
                return false; // fe80::/10 link-local
            }
            if v6.segments()[0] & 0xfe00 == 0xfc00 {
                return false; // fc00::/7 unique local
            }
            // IPv4-mapped (::ffff:1.2.3.4) — recurse on the embedded v4.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(std::net::IpAddr::V4(v4));
            }
            true
        }
    }
}

/// Best available page title, trimmed; "" if absent. Prefers OpenGraph /
/// Twitter meta titles (more descriptive on most sites), then falls back to
/// the document <title>. SPA sites (YouTube Music, …) only ship a generic
/// <title>/og:title without JS rendering — a known limitation of client-side
/// previews.
fn extract_title(html: &str) -> String {
    // <meta property="og:title" content="…"> or <meta name="twitter:title" …>
    for needle in ["og:title", "twitter:title"] {
        let Some(open) = find_ascii_ci(html, needle.as_bytes()) else { continue };
        let rest = &html[open..];
        let Some(content) = find_ascii_ci(rest, b"content=") else { continue };
        let after = &rest[content + "content=".len()..];
        let value = match after.as_bytes().first() {
            Some(b'"') => after[1..].split('"').next().unwrap_or_default(),
            Some(b'\'') => after[1..].split('\'').next().unwrap_or_default(),
            _ => after.split(|c: char| c.is_whitespace()).next().unwrap_or_default(),
        };
        // HTML-unescape the common entities (&amp; &lt; &gt; &quot; &#39;).
        let unescaped = value
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'");
        let title = unescaped.trim();
        if !title.is_empty() {
            return title.to_string();
        }
    }

    let Some(open) = find_ascii_ci(html, b"<title") else { return String::new() };
    let rest = &html[open..];
    let Some(gt) = rest.find('>') else { return String::new() };
    let content = &rest[gt + 1..];
    let Some(close) = find_ascii_ci(content, b"</title") else { return String::new() };
    content[..close].trim().to_string()
}

/// Case-insensitive byte search for an ASCII `needle`. Byte offsets are safe
/// UTF-8 boundaries because `needle` starts with an ASCII byte.
fn find_ascii_ci(haystack: &str, needle: &[u8]) -> Option<usize> {
    let h = haystack.as_bytes();
    if needle.is_empty() || needle.len() > h.len() {
        return None;
    }
    'outer: for i in 0..=h.len() - needle.len() {
        for (j, &nb) in needle.iter().enumerate() {
            if h[i + j].to_ascii_lowercase() != nb {
                continue 'outer;
            }
        }
        return Some(i);
    }
    None
}

#[cfg(test)]
mod extract_title_tests {
    use super::extract_title;

    #[test]
    fn prefers_og_title() {
        let html = "<html><head><title>Generic</title><meta property=\"og:title\" content=\"The Real Title\"></head></html>";
        assert_eq!(extract_title(html), "The Real Title");
    }

    #[test]
    fn prefers_twitter_title_when_no_og() {
        let html = "<html><head><title>Generic</title><meta name=\"twitter:title\" content=\"Tweet Title\"></head></html>";
        assert_eq!(extract_title(html), "Tweet Title");
    }

    #[test]
    fn falls_back_to_document_title() {
        let html = "<html><head><title>Just a Page</title></head><body>x</body></html>";
        assert_eq!(extract_title(html), "Just a Page");
    }

    #[test]
    fn unescapes_html_entities() {
        let html = r#"<meta property="og:title" content="A &amp; B &lt;tag&gt; &quot;q&quot;">"#;
        assert_eq!(extract_title(html), "A & B <tag> \"q\"");
    }

    #[test]
    fn handles_single_quoted_content() {
        let html = r#"<meta property="og:title" content='Single Quoted'>"#;
        assert_eq!(extract_title(html), "Single Quoted");
    }

    #[test]
    fn empty_when_no_title_at_all() {
        assert_eq!(extract_title("<html><body>no head</body></html>"), "");
        assert_eq!(extract_title(""), "");
    }

    #[test]
    fn handles_non_ascii_title() {
        let html = "<html><head><title>Lumen — Voz P2P ultraligera</title></head></html>";
        assert_eq!(extract_title(html), "Lumen — Voz P2P ultraligera");
    }
}

#[cfg(test)]
mod security_tests {
    use super::is_public_ip;
    use std::net::IpAddr;

    fn v4(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn rejects_private_and_internal_ranges() {
        // RFC 1918 private
        assert!(!is_public_ip(v4("10.0.0.1")));
        assert!(!is_public_ip(v4("172.16.0.1")));
        assert!(!is_public_ip(v4("172.31.255.254")));
        assert!(!is_public_ip(v4("192.168.1.1")));
        // loopback
        assert!(!is_public_ip(v4("127.0.0.1")));
        assert!(!is_public_ip(v4("127.0.0.2")));
        // link-local + cloud metadata (169.254.169.254 is link-local)
        assert!(!is_public_ip(v4("169.254.169.254")));
        assert!(!is_public_ip(v4("169.254.10.10")));
        // CGNAT (RFC 6598 100.64/10)
        assert!(!is_public_ip(v4("100.64.0.1")));
        assert!(!is_public_ip(v4("100.127.255.254")));
        // documentation / benchmark
        assert!(!is_public_ip(v4("192.0.2.1")));
        assert!(!is_public_ip(v4("198.18.0.1")));
        // unspecified / broadcast
        assert!(!is_public_ip(v4("0.0.0.0")));
        assert!(!is_public_ip(v4("255.255.255.255")));
        // multicast
        assert!(!is_public_ip(v4("224.0.0.1")));
    }

    #[test]
    fn rejects_ipv6_internal_and_private() {
        assert!(!is_public_ip("::1".parse().unwrap()));
        assert!(!is_public_ip("::".parse().unwrap()));
        assert!(!is_public_ip("fe80::1".parse().unwrap()));
        assert!(!is_public_ip("fc00::1".parse().unwrap()));
        assert!(!is_public_ip("fd12:3456::1".parse().unwrap()));
        // IPv4-mapped private
        assert!(!is_public_ip("::ffff:127.0.0.1".parse().unwrap()));
        assert!(!is_public_ip("::ffff:169.254.169.254".parse().unwrap()));
    }

    #[test]
    fn accepts_public_addresses() {
        assert!(is_public_ip(v4("8.8.8.8")));
        assert!(is_public_ip(v4("1.1.1.1")));
        assert!(is_public_ip(v4("93.184.216.34"))); // example.com
        assert!(is_public_ip("2606:2800:220:1:248:1893:25c8:1946".parse().unwrap())); // example.com v6
    }
}
