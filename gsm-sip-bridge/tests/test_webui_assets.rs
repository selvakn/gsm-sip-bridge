// specs/122-management-webui, FR-044/FR-070: the embedded page may never build
// HTML from a string, never carry inline script or style (the CSP forbids
// both), and stays small.

const APP_JS: &str = include_str!("../src/webui/assets/app.js");
const INDEX_HTML: &str = include_str!("../src/webui/assets/index.html");
const STYLE_CSS: &str = include_str!("../src/webui/assets/style.css");

/// SMS bodies and caller IDs are written by strangers. Every one of these
/// turns a string into markup.
#[test]
fn app_js_has_no_html_sinks() {
    for sink in [
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
        "createContextualFragment",
        "srcdoc",
    ] {
        assert!(!APP_JS.contains(sink), "app.js must not use {sink}");
    }
}

/// Code that runs a string is the same hole by another door.
#[test]
fn app_js_does_not_evaluate_strings() {
    for sink in ["eval(", "new Function(", "setTimeout(\"", "setInterval(\""] {
        assert!(!APP_JS.contains(sink), "app.js must not use {sink}");
    }
}

#[test]
fn index_html_carries_no_inline_script_or_style() {
    // A <script> is fine only as `<script src=...></script>`: no body.
    let mut rest = INDEX_HTML;
    while let Some(start) = rest.find("<script") {
        let tag_end = rest[start..]
            .find('>')
            .expect("an unterminated <script> tag")
            + start;
        let tag = &rest[start..tag_end];
        assert!(tag.contains("src="), "inline <script> in index.html: {tag}");
        let close = rest[tag_end..]
            .find("</script>")
            .expect("an unclosed <script>")
            + tag_end;
        assert!(
            rest[tag_end + 1..close].trim().is_empty(),
            "a <script> with a body in index.html"
        );
        rest = &rest[close..];
    }
    assert!(
        !INDEX_HTML.contains("<style"),
        "inline <style> in index.html"
    );
    assert!(
        !INDEX_HTML.contains(" style="),
        "style= attribute in index.html"
    );
    for handler in [" onclick=", " onload=", " onerror="] {
        assert!(
            !INDEX_HTML.contains(handler),
            "inline event handler {handler}"
        );
    }
}

#[test]
fn app_js_sets_no_style_attribute() {
    // The CSP's `style-src 'self'` blocks inline styles, so one would only
    // ever silently fail.
    assert!(
        !APP_JS.contains("style:"),
        "app.js passes a style attribute"
    );
    assert!(!APP_JS.contains(".style."), "app.js sets an inline style");
    assert!(!APP_JS.contains("cssText"), "app.js sets cssText");
}

#[test]
fn the_three_assets_stay_small() {
    let total = APP_JS.len() + INDEX_HTML.len() + STYLE_CSS.len();
    assert!(
        total <= 150 * 1024,
        "the embedded assets are {total} bytes; the budget is 150 KB"
    );
}

#[test]
fn the_stylesheet_uses_the_project_palette_and_follows_the_color_scheme() {
    // Dark mode is a real second palette, not an afterthought.
    assert!(STYLE_CSS.contains("prefers-color-scheme: dark"));
    for colour in [
        "#001219", "#005f73", "#0a9396", "#94d2bd", "#e9d8a6", "#ee9b00", "#ca6702", "#bb3e03",
        "#ae2012",
    ] {
        assert!(
            STYLE_CSS.contains(colour),
            "palette colour {colour} is not used"
        );
    }
}

#[test]
fn nothing_loads_from_another_origin() {
    for asset in [APP_JS, INDEX_HTML, STYLE_CSS] {
        for scheme in ["http://", "https://", "//cdn"] {
            // The word may appear in prose or in a user-supplied link target
            // (the Grafana URL is data, not an asset).
            let hits: Vec<&str> = asset
                .lines()
                .filter(|l| l.contains(scheme))
                .filter(|l| !l.trim_start().starts_with("//") && !l.trim_start().starts_with("/*"))
                .collect();
            assert!(hits.is_empty(), "an external reference: {hits:?}");
        }
    }
}
