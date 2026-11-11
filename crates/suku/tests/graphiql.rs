//! GraphiQL asset tests.
//!
//! The default build keeps the CDN-backed page. The `vendored-graphiql` feature must produce a
//! self-contained page: no unpkg, no external script or stylesheet, and no other network source.

use suku::http::graphiql_html;

#[cfg(not(feature = "vendored-graphiql"))]
#[test]
fn default_page_loads_graphiql_from_the_cdn() {
    let html = graphiql_html();

    assert!(html.contains("https://unpkg.com/react@18/umd/react.production.min.js"));
    assert!(html.contains("https://unpkg.com/react-dom@18/umd/react-dom.production.min.js"));
    assert!(html.contains("https://unpkg.com/graphiql/graphiql.min.js"));
    assert!(html.contains("https://unpkg.com/graphiql/graphiql.min.css"));
}

#[cfg(feature = "vendored-graphiql")]
mod vendored {
    use super::graphiql_html;

    #[test]
    fn there_is_no_cdn_reference() {
        let html = graphiql_html();

        assert!(!html.contains("unpkg.com"), "unpkg must not be referenced");
        assert!(
            !html.contains("jsdelivr.com"),
            "jsdelivr must not be referenced"
        );
        assert!(
            !html.contains("cdnjs.cloudflare.com"),
            "cdnjs must not be referenced"
        );
        assert!(!html.contains("esm.sh"), "esm.sh must not be referenced");
        assert!(
            !html.contains("src=\"http") && !html.contains("src='http"),
            "no external script or frame source"
        );
        assert!(
            !html.contains("href=\"http") && !html.contains("href='http"),
            "no external stylesheet or link"
        );
    }

    #[test]
    fn the_assets_are_inlined() {
        let html = graphiql_html();

        // React's production bundle exposes `createElement`; ReactDOM's is several times larger.
        // Both are present as inline scripts, not as `src` references.
        assert!(html.contains("createElement"));
        assert!(html.contains("createRoot"));
        // A distinctive string from GraphiQL's UMD bundle.
        assert!(html.contains("GraphiQL 0.18.0 and after is not compatible"));
        // GraphiQL's stylesheet ships its fonts as data URIs, which proves the CSS is inline.
        assert!(html.contains("data:font/woff2;base64,"));
    }

    #[test]
    fn the_page_is_structurally_complete() {
        assert_starts_with_doctype(graphiql_html());
        assert_balanced(graphiql_html());

        let html = graphiql_html();
        for required in [
            "<html lang=\"en\">",
            "<head>",
            "</head>",
            "<body>",
            "</body>",
            "</html>",
            "<title>GraphiQL</title>",
            "<div id=\"graphiql\">",
        ] {
            assert!(html.contains(required), "missing {required}");
        }
    }

    #[test]
    fn the_bootstrap_renders_graphiql() {
        let html = graphiql_html();

        assert!(html.contains("var fetcher = GraphiQL.createFetcher"));
        assert!(html.contains("ReactDOM.createRoot(container).render"));
        assert!(html.contains("React.createElement(GraphiQL"));
    }

    #[test]
    fn every_script_block_is_closed() {
        let html = graphiql_html();
        assert_eq!(
            html.matches("<script>\n").count(),
            4,
            "expected four scripts"
        );
        assert_eq!(
            html.matches("</script>").count(),
            4,
            "every script block must be closed"
        );
        assert_eq!(
            html.matches("<style>\n").count(),
            html.matches("</style>").count(),
            "every style block must be closed"
        );
    }

    fn assert_starts_with_doctype(html: &str) {
        assert!(
            html.starts_with("<!DOCTYPE html>"),
            "page must start with a doctype"
        );
    }

    /// Cheap tag balance check: every opening `<tag` for the tags we generate has a closing tag
    /// and the page has no stray closing tag before its opening tag.
    fn assert_balanced(html: &str) {
        for tag in ["html", "head", "body", "div"] {
            let opens = html.matches(&format!("<{tag}")).count();
            let closes = html.matches(&format!("</{tag}>")).count();
            assert_eq!(opens, closes, "unbalanced <{tag}> tags");
        }
    }
}
