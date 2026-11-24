//! Web search and page reading for the model, without keys or accounts.
//!
//! - **Search** asks DuckDuckGo's plain HTML results page and returns
//!   titles, addresses and snippets.
//! - **Reading** fetches one page (http or https), keeps its readable text
//!   (no scripts, styles or navigation), and pages through long text.
//!
//! Everything that comes back is outside content: the chat loop registers it
//! with the origin gate, so actions traced to it are held for the user. A
//! search query, or an address with a query string, carries text to another
//! site, so it is proposed as a network send: refused when it was traced to
//! outside content, not to the user's own words.
use std::time::Duration;
use ureq::ResponseExt;

/// Most bytes read from one page.
const MAX_PAGE_BYTES: u64 = 5 * 1024 * 1024;
/// Characters of page text returned in one piece.
pub const PAGE_CHARS: usize = 20_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// The address after redirects.
    pub url: String,
    pub title: String,
    /// The readable text (all of it; callers page through it).
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebError {
    /// Not an http(s) address, or one Lu does not fetch (this PC, local network).
    Refused(String),
    Network(String),
    Status(u16),
    /// Not a text page (an image, a download, ...).
    NotText(String),
}
impl std::fmt::Display for WebError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(w) => write!(f, "not fetched: {w}"),
            Self::Network(e) => write!(f, "the site could not be reached: {e}"),
            Self::Status(s) => write!(f, "the site answered {s}"),
            Self::NotText(t) => write!(f, "not a web page ({t}); download it with a command if it is needed"),
        }
    }
}
impl std::error::Error for WebError {}

pub struct Web {
    agent: ureq::Agent,
    search_url: String,
    allow_local: bool,
}

impl Web {
    pub fn new() -> Self {
        Self { agent: agent(), search_url: "https://html.duckduckgo.com/html/".into(), allow_local: false }
    }
    /// For tests: a stand-in search page, and local addresses allowed.
    pub fn with_search_url(search_url: &str) -> Self {
        Self { agent: agent(), search_url: search_url.into(), allow_local: true }
    }

    /// Search the web; at most `max` results, ads left out.
    pub fn search(&self, query: &str, max: usize) -> Result<Vec<SearchResult>, WebError> {
        let url = format!("{}?q={}", self.search_url, encode(query.trim()));
        let (_, html, _) = self.get(&url)?;
        Ok(parse_results(&html, max.clamp(1, 20)))
    }

    /// Fetch a page and keep its readable text.
    pub fn read(&self, url: &str) -> Result<Page, WebError> {
        let (final_url, body, _) = self.get(url)?;
        let title = between(&body, "<title", "</title>").map(|t| clean(t.split_once('>').map_or(t, |x| x.1))).unwrap_or_default();
        Ok(Page { url: final_url, title, text: html_to_text(&body) })
    }

    fn get(&self, url: &str) -> Result<(String, String, String), WebError> {
        check_address(url, self.allow_local)?;
        let resp = self.agent.get(url).header("Accept", "text/html,text/plain;q=0.9,*/*;q=0.1").call().map_err(|e| WebError::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(WebError::Status(status));
        }
        let ctype = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
        if !(ctype.is_empty() || ctype.contains("html") || ctype.starts_with("text/") || ctype.contains("xml") || ctype.contains("json")) {
            return Err(WebError::NotText(ctype));
        }
        let final_url = resp.get_uri().to_string();
        // Redirects may not lead somewhere Lu would not fetch directly.
        check_address(&final_url, self.allow_local)?;
        let body = resp.into_body().with_config().limit(MAX_PAGE_BYTES).lossy_utf8(true).read_to_string().map_err(|e| WebError::Network(e.to_string()))?;
        Ok((final_url, body, ctype))
    }
}
impl Default for Web {
    fn default() -> Self {
        Self::new()
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(30)))
        .max_redirects(5)
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) Lu/0.1")
        .build()
        .into()
}

/// Only http(s), and not this PC or the local network (unless `allow_local`).
pub fn check_address(url: &str, allow_local: bool) -> Result<(), WebError> {
    let lower = url.trim().to_ascii_lowercase();
    let rest = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://")).ok_or_else(|| WebError::Refused("only http and https addresses".into()))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return Err(WebError::Refused("addresses with a user name are not fetched".into()));
    }
    let host = if let Some(h) = authority.strip_prefix('[') { h.split(']').next().unwrap_or("") } else { authority.rsplit_once(':').map_or(authority, |(h, p)| if p.chars().all(|c| c.is_ascii_digit()) { h } else { authority }) };
    if host.is_empty() {
        return Err(WebError::Refused("no site in the address".into()));
    }
    if allow_local {
        return Ok(());
    }
    let local = host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || match host.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V4(ip)) => ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified() || ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1]),
            Ok(std::net::IpAddr::V6(ip)) => ip.is_loopback() || ip.is_unspecified() || (ip.segments()[0] & 0xfe00) == 0xfc00 || (ip.segments()[0] & 0xffc0) == 0xfe80,
            Err(_) => false,
        };
    if local {
        return Err(WebError::Refused("addresses on this PC or the local network are not fetched".into()));
    }
    Ok(())
}

/// Whether fetching this address sends text to the site (a query string).
pub fn sends_text(url: &str) -> bool {
    url.contains('?')
}

/// Percent-encode for a query value.
pub fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
/// Percent-decode (and '+' as space).
pub fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() && b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit() => {
                let hex = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
                out.push(hex(b[i + 1]) * 16 + hex(b[i + 2]));
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let lower = s.to_ascii_lowercase();
    let a = lower.find(open)?;
    let b = lower[a..].find(close)? + a;
    Some(&s[a + open.len()..b])
}

/// The value of an attribute inside one tag's text.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(p) = lower[from..].find(name) {
        let at = from + p;
        let before_ok = at == 0 || !lower.as_bytes()[at - 1].is_ascii_alphanumeric() && lower.as_bytes()[at - 1] != b'-';
        let rest = lower[at + name.len()..].trim_start();
        if before_ok && rest.starts_with('=') {
            let start = tag.len() - rest.len() + 1;
            let v = tag[start..].trim_start();
            let q = v.chars().next()?;
            return if q == '"' || q == '\'' { v[1..].split(q).next() } else { v.split(|c: char| c.is_whitespace() || c == '>').next() };
        }
        from = at + name.len();
    }
    None
}

/// Results from DuckDuckGo's HTML page.
pub fn parse_results(html: &str, max: usize) -> Vec<SearchResult> {
    let mut out: Vec<SearchResult> = Vec::new();
    let lower = html.to_ascii_lowercase();
    let mut pos = 0;
    while out.len() < max {
        let Some(p) = lower[pos..].find("class=\"result__a\"") else { break };
        let at = pos + p;
        let tag_start = lower[..at].rfind('<').unwrap_or(at);
        let tag_end = lower[at..].find('>').map_or(lower.len(), |e| at + e);
        let tag = &html[tag_start..tag_end];
        let close = lower[tag_end..].find("</a>").map_or(lower.len(), |e| tag_end + e);
        let title = clean(&html[(tag_end + 1).min(close)..close]);
        // The next snippet before the next result.
        let next = lower[close..].find("class=\"result__a\"").map_or(lower.len(), |e| close + e);
        let snippet = lower[close..next].find("result__snippet").and_then(|s| {
            let s = close + s;
            let open = lower[s..].find('>').map(|e| s + e + 1)?;
            let end = lower[open..].find("</a>").or_else(|| lower[open..].find("</td>")).or_else(|| lower[open..].find("</div>")).map(|e| open + e)?;
            Some(clean(&html[open..end.min(next)]))
        });
        pos = close;
        let Some(href) = attr(tag, "href") else { continue };
        let href = entities(href);
        // Ads go through y.js; real results through /l/?uddg=<address>.
        if href.contains("/y.js") || href.contains("ad_domain") {
            continue;
        }
        let url = match href.split_once("uddg=") {
            Some((_, v)) => decode(v.split('&').next().unwrap_or(v)),
            None if href.starts_with("//") => format!("https:{href}"),
            None => href,
        };
        if !url.starts_with("http") || out.iter().any(|r| r.url == url) {
            continue;
        }
        out.push(SearchResult { title, url, snippet: snippet.unwrap_or_default() });
    }
    out
}

/// Tags stripped of markup, entities decoded, whitespace collapsed.
fn clean(fragment: &str) -> String {
    let mut s = String::new();
    let mut in_tag = false;
    for c in fragment.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => s.push(c),
            _ => {}
        }
    }
    entities(&s).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// HTML character references.
pub fn entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        let end = rest[1..].find(|c: char| c == ';' || c == '&' || c.is_whitespace()).map(|e| e + 1);
        let decoded = match end {
            Some(e) if rest.as_bytes().get(e) == Some(&b';') && e <= 12 => {
                let name = &rest[1..e];
                let ch = match name {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" | "#39" => Some('\''),
                    "nbsp" => Some(' '),
                    "mdash" => Some('—'),
                    "ndash" => Some('–'),
                    "hellip" => Some('…'),
                    "rsquo" => Some('’'),
                    "lsquo" => Some('‘'),
                    "rdquo" => Some('”'),
                    "ldquo" => Some('“'),
                    "copy" => Some('©'),
                    _ => name
                        .strip_prefix("#x")
                        .or_else(|| name.strip_prefix("#X"))
                        .and_then(|h| u32::from_str_radix(h, 16).ok())
                        .or_else(|| name.strip_prefix('#').and_then(|d| d.parse().ok()))
                        .and_then(char::from_u32),
                };
                ch.map(|c| (c, e + 1))
            }
            _ => None,
        };
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The readable text of a page: scripts, styles, navigation, headers,
/// footers and forms dropped; blocks on their own lines; links kept as
/// "text (address)" when the address is absolute.
pub fn html_to_text(html: &str) -> String {
    const SKIP: [&str; 10] = ["head", "script", "style", "noscript", "svg", "nav", "footer", "header", "form", "template"];
    const BLOCK: [&str; 22] = ["p", "div", "br", "li", "tr", "h1", "h2", "h3", "h4", "h5", "h6", "section", "article", "table", "ul", "ol", "pre", "blockquote", "dt", "dd", "hr", "main"];
    let mut out = String::new();
    let mut skip: Option<String> = None;
    let mut link: Option<String> = None;
    let mut i = 0;
    let b = html.as_bytes();
    while i < b.len() {
        if b[i] == b'<' {
            if html[i..].starts_with("<!--") {
                i = html[i..].find("-->").map_or(b.len(), |e| i + e + 3);
                continue;
            }
            let end = html[i..].find('>').map_or(b.len(), |e| i + e);
            let tag = &html[i + 1..end.min(b.len())];
            let closing = tag.starts_with('/');
            let name: String = tag.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
            i = end + 1;
            if let Some(s) = &skip {
                if closing && name == *s {
                    skip = None;
                }
                continue;
            }
            if !closing && SKIP.contains(&name.as_str()) && !tag.ends_with('/') {
                skip = Some(name);
                continue;
            }
            if name == "a" {
                if closing {
                    if let Some(h) = link.take() {
                        out.push_str(&format!(" ({h})"));
                    }
                } else {
                    link = attr(tag, "href").map(entities).filter(|h| h.starts_with("http"));
                }
            }
            if BLOCK.contains(&name.as_str()) {
                out.push('\n');
            } else if (name == "td" || name == "th") && !closing {
                // Cells on one line, separated, without a leading bar.
                if !out.rsplit('\n').next().unwrap_or("").trim().is_empty() {
                    out.push_str(" | ");
                }
            }
            continue;
        }
        if skip.is_some() {
            i += 1;
            continue;
        }
        let next = html[i..].find('<').map_or(b.len(), |e| i + e);
        out.push_str(&entities(&html[i..next]));
        i = next;
    }
    // Collapse spaces within lines and blank runs between them.
    let mut text = String::new();
    let mut blank = 0;
    for line in out.lines() {
        let l = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if l.is_empty() || l == "|" {
            blank += 1;
            continue;
        }
        if !text.is_empty() {
            text.push_str(if blank > 0 { "\n\n" } else { "\n" });
        }
        blank = 0;
        text.push_str(&l);
    }
    text
}

/// One piece of a page's text, for the model: from character `start`, at
/// most [`PAGE_CHARS`], with where to continue.
pub fn page_piece(page: &Page, start: usize) -> String {
    let total = page.text.chars().count();
    let start = start.min(total);
    let piece: String = page.text.chars().skip(start).take(PAGE_CHARS).collect();
    let end = start + piece.chars().count();
    let mut s = format!("{}\n{}\n(characters {start}-{end} of {total})\n\n{piece}", page.title, page.url);
    if end < total {
        s.push_str(&format!("\n\n(more: read again with start={end})"));
    }
    s
}
