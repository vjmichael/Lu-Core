use lu_core::web::{check_address, decode, encode, entities, html_to_text, page_piece, parse_results, Page, Web, PAGE_CHARS};
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
};

/// A tiny local site: /html/ is a search results page, /page is an article,
/// /img is an image, /go redirects to /page.
fn site() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let me = base.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = req.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let (ctype, body, extra) = if path.starts_with("/html/") {
                let q = path.split("q=").nth(1).unwrap_or("");
                ("text/html", results_page(&me, q), String::new())
            } else if path == "/page" {
                ("text/html; charset=utf-8", ARTICLE.to_owned(), String::new())
            } else if path == "/img" {
                ("image/png", "PNG".to_owned(), String::new())
            } else if path == "/go" {
                ("text/html", String::new(), format!("Location: {me}/page\r\n"))
            } else {
                ("text/html", "nope".to_owned(), String::new())
            };
            let status = if path == "/go" { "302 Found" } else if body == "nope" { "404 Not Found" } else { "200 OK" };
            let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}", body.len());
        }
    });
    base
}

fn results_page(base: &str, q: &str) -> String {
    format!(
        r##"<html><body>
<div class="result results_links"><a rel="nofollow" class="result__a" href="//duckduckgo.com/y.js?ad_domain=shop.example&amp;u=x">Buy pumpkins now</a>
<a class="result__snippet" href="#">An ad.</a></div>
<div class="result"><h2><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg={}&amp;rut=abc">Pumpkin <b>Carving</b> &amp; Patterns</a></h2>
<a class="result__snippet" href="x">Free patterns for <b>{q}</b>, over 1,000 stencils.</a></div>
<div class="result"><h2><a rel="nofollow" class="result__a" href="https://example.org/two">Second &#8211; result</a></h2>
<a class="result__snippet" href="y">Another one.</a></div>
</body></html>"##,
        encode(&format!("{base}/page"))
    )
}

const ARTICLE: &str = r#"<!doctype html><html><head><title>Carving &amp; Lanterns</title>
<style>body{color:red}</style><script>alert("x")</script></head>
<body><header>Site menu</header><nav><a href="/a">Home</a></nav>
<main><h1>How to carve</h1><p>Cut the <b>top</b> off&nbsp;first.</p>
<p>See <a href="https://example.org/more?id=7">more tips</a> here.</p>
<table><tr><th>Tool</th><th>Use</th></tr><tr><td>Knife</td><td>Cutting</td></tr></table>
<!-- a comment --></main><footer>Copyright</footer></body></html>"#;

#[test]
fn search_results_are_parsed_and_ads_left_out() {
    let base = site();
    let web = Web::with_search_url(&format!("{base}/html/"));
    let r = web.search("jack o lantern", 10).unwrap();
    assert_eq!(r.len(), 2, "{r:?}");
    assert_eq!(r[0].title, "Pumpkin Carving & Patterns");
    assert_eq!(r[0].url, format!("{base}/page"), "the DuckDuckGo redirect is unwrapped");
    assert!(r[0].snippet.contains("jack+o+lantern") || r[0].snippet.contains("jack o lantern"), "{}", r[0].snippet);
    assert_eq!(r[1].title, "Second – result");
    assert_eq!(r[1].url, "https://example.org/two");
    assert_eq!(parse_results("<html>nothing</html>", 5), vec![]);
}

#[test]
fn pages_are_read_as_text_without_scripts_or_menus() {
    let base = site();
    let web = Web::with_search_url(&format!("{base}/html/"));
    let p = web.read(&format!("{base}/page")).unwrap();
    assert_eq!(p.title, "Carving & Lanterns");
    assert!(p.text.contains("How to carve"));
    assert!(p.text.contains("Cut the top off first."), "{}", p.text);
    assert!(p.text.contains("more tips (https://example.org/more?id=7)"), "{}", p.text);
    assert!(p.text.contains("Knife | Cutting"), "{}", p.text);
    for gone in ["alert", "color:red", "Site menu", "Home", "Copyright", "comment"] {
        assert!(!p.text.contains(gone), "{gone} should be gone: {}", p.text);
    }
    // Redirects are followed; images are not read as text; errors are plain.
    assert_eq!(web.read(&format!("{base}/go")).unwrap().title, "Carving & Lanterns");
    assert!(web.read(&format!("{base}/img")).unwrap_err().to_string().contains("not a web page"));
    assert!(web.read(&format!("{base}/missing")).unwrap_err().to_string().contains("404"));
}

#[test]
fn only_public_web_addresses_are_fetched() {
    for ok in ["https://example.org/a?b=1", "http://news.example.com:8080/x", "https://[2606:4700::1]/"] {
        assert!(check_address(ok, false).is_ok(), "{ok}");
    }
    for bad in [
        "file:///C:/Windows/win.ini",
        "ftp://example.org/",
        "http://localhost:8080/",
        "http://127.0.0.1:61428/v1/models",
        "http://192.168.1.10/",
        "http://10.0.0.5/admin",
        "http://169.254.169.254/latest/meta-data",
        "http://[::1]/",
        "http://printer.local/",
        "https://user:pw@example.org/",
    ] {
        assert!(check_address(bad, false).is_err(), "{bad}");
    }
    assert!(Web::new().read("http://127.0.0.1:1/").unwrap_err().to_string().contains("not fetched"));
}

#[test]
fn text_helpers() {
    assert_eq!(entities("a &amp; b &lt;c&gt; &#39;d&#x27; &bogus; & e"), "a & b <c> 'd' &bogus; & e");
    assert_eq!(encode("a b&c=ü"), "a+b%26c%3D%C3%BC");
    assert_eq!(decode("a+b%26c%3D%C3%BC%zz"), "a b&c=ü%zz");
    assert_eq!(html_to_text("<p>One</p><p>Two</p><br>Three"), "One\n\nTwo\n\nThree");
    let long = Page { url: "https://x".into(), title: "T".into(), text: "x".repeat(PAGE_CHARS + 10) };
    let first = page_piece(&long, 0);
    assert!(first.contains(&format!("start={PAGE_CHARS}")));
    let rest = page_piece(&long, PAGE_CHARS);
    assert!(rest.contains(&format!("characters {PAGE_CHARS}-{} of {}", PAGE_CHARS + 10, PAGE_CHARS + 10)) && !rest.contains("more:"));
}

/// Owner machine / network: a real search and a real page.
#[test]
#[ignore]
fn real_search_and_page() {
    let web = Web::new();
    let r = web.search("llama.cpp github", 5).unwrap();
    for x in &r {
        println!("{} | {} | {}", x.title, x.url, x.snippet);
    }
    assert!(!r.is_empty(), "no results");
    let p = web.read(&r[0].url).unwrap();
    println!("PAGE {} ({} chars)\n{}", p.title, p.text.len(), p.text.chars().take(600).collect::<String>());
    assert!(p.text.len() > 200);
}

#[test]
fn pages_list_their_real_pictures() {
    let html = r#"<html><body><p>Film</p><img src="//upload.wikimedia.org/a/b/Ship.jpg" alt="The ship"><img src="https://x.org/logo.png"><img src="https://x.org/p.gif" width="1" height="1"><img src="/rel.jpg"></body></html>"#;
    let t = html_to_text(html);
    assert!(t.contains("![The ship](https://upload.wikimedia.org/a/b/Ship.jpg)"), "{t}");
    assert!(!t.contains("logo") && !t.contains("p.gif"), "{t}");
}

#[test]
#[ignore]
fn real_page_pictures() {
    let p = Web::new().read("https://commons.wikimedia.org/wiki/File:Johnny_Depp_as_Captain_Jack_Sparrow_in_Queensland,_Australia.jpg").unwrap();
    for l in p.text.lines().filter(|l| l.starts_with("![")).take(6) {
        println!("{l}");
    }
}
