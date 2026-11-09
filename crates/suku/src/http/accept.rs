//! `Accept` header negotiation.
//!
//! GraphQL-over-HTTP has four relevant media types: the legacy `application/json` response,
//! `application/graphql-response+json`, `text/event-stream` for subscriptions and `text/html` for
//! GraphiQL. The parser only needs quality values, so it intentionally ignores every other
//! media-range parameter.

use ::http::{HeaderMap, header};

/// Which JSON media type a response should use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JsonMedia {
    /// `application/json`; request errors are still returned with status 200.
    Legacy,
    /// `application/graphql-response+json`; request errors may use status 400.
    GraphQLResponse,
}

impl JsonMedia {
    pub(crate) const fn content_type(self) -> &'static str {
        match self {
            Self::Legacy => "application/json; charset=utf-8",
            Self::GraphQLResponse => "application/graphql-response+json; charset=utf-8",
        }
    }
}

/// The negotiated capabilities of one request.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Accept {
    /// The preferred JSON media type, when JSON is acceptable.
    pub(crate) json: Option<JsonMedia>,
    /// Whether `text/event-stream` is acceptable.
    pub(crate) event_stream: bool,
    /// Whether `text/html` is acceptable.
    pub(crate) html: bool,
}

impl Accept {
    pub(crate) const fn json_or_legacy(self) -> JsonMedia {
        match self.json {
            Some(media) => media,
            None => JsonMedia::Legacy,
        }
    }
}

/// Parse `Accept` headers into the capabilities this server understands.
pub(crate) fn parse(headers: &HeaderMap) -> Accept {
    let mut values = headers.get_all(header::ACCEPT).iter().peekable();
    if values.peek().is_none() {
        return Accept {
            json: Some(JsonMedia::Legacy),
            event_stream: false,
            html: false,
        };
    }

    let mut graphql_q: Option<f32> = None;
    let mut json_q: Option<f32> = None;
    let mut wildcard_q: Option<f32> = None;
    let mut event_q: Option<f32> = None;
    let mut html_q: Option<f32> = None;
    let mut text_wildcard_q: Option<f32> = None;

    for value in values {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for item in value.split(',') {
            let mut parts = item.split(';');
            let range = parts.next().unwrap_or("").trim().to_ascii_lowercase();
            if range.is_empty() {
                continue;
            }

            let mut quality = 1.0_f32;
            for parameter in parts {
                let parameter = parameter.trim();
                if let Some((name, value)) = parameter.split_once('=') {
                    if name.trim().eq_ignore_ascii_case("q") {
                        quality = value.trim().parse::<f32>().unwrap_or(1.0).clamp(0.0, 1.0);
                        break;
                    }
                }
            }

            match range.as_str() {
                "application/graphql-response+json" => {
                    graphql_q = Some(graphql_q.map_or(quality, |current| current.max(quality)));
                }
                "application/json" => {
                    json_q = Some(json_q.map_or(quality, |current| current.max(quality)));
                }
                "application/*" | "*/*" => {
                    wildcard_q = Some(wildcard_q.map_or(quality, |current| current.max(quality)));
                }
                "text/event-stream" => {
                    event_q = Some(event_q.map_or(quality, |current| current.max(quality)));
                }
                "text/html" => {
                    html_q = Some(html_q.map_or(quality, |current| current.max(quality)));
                }
                "text/*" => {
                    text_wildcard_q =
                        Some(text_wildcard_q.map_or(quality, |current| current.max(quality)));
                }
                _ => {}
            }
        }
    }

    // A specific range wins over a wildcard, including when its quality is zero (an explicit
    // rejection). A wildcard that is not overridden also makes the modern JSON type acceptable,
    // which matters when a client explicitly rejects only `application/json`.
    let wildcard_quality = wildcard_q.unwrap_or(0.0);
    let legacy_quality = json_q.unwrap_or(wildcard_quality);
    let graphql_quality = graphql_q.unwrap_or(wildcard_quality);
    let json = if graphql_quality > 0.0
        && (graphql_quality > legacy_quality
            || (graphql_quality == legacy_quality && graphql_q.is_some()))
    {
        Some(JsonMedia::GraphQLResponse)
    } else if legacy_quality > 0.0 {
        Some(JsonMedia::Legacy)
    } else {
        None
    };

    let event_stream = event_q.or(text_wildcard_q).unwrap_or(0.0) > 0.0;
    // `*/*` also matches HTML, but a browser that wants GraphiQL always lists `text/html`
    // explicitly. Requiring it prevents `curl` (no Accept header) from getting a web page.
    let html = html_q.unwrap_or(0.0) > 0.0;

    Accept {
        json,
        event_stream,
        html,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::http::HeaderValue;

    fn accept(value: &'static str) -> Accept {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, HeaderValue::from_static(value));
        parse(&headers)
    }

    #[test]
    fn missing_header_defaults_to_legacy_json() {
        let accept = parse(&HeaderMap::new());
        assert_eq!(accept.json, Some(JsonMedia::Legacy));
        assert!(!accept.event_stream);
        assert!(!accept.html);
    }

    #[test]
    fn wildcard_defaults_to_legacy_json() {
        assert_eq!(accept("*/*").json, Some(JsonMedia::Legacy));
        assert!(!accept("*/*").html);
    }

    #[test]
    fn explicit_modern_type_wins_a_tie() {
        assert_eq!(
            accept("application/json, application/graphql-response+json").json,
            Some(JsonMedia::GraphQLResponse)
        );
    }

    #[test]
    fn quality_values_select_the_higher_type() {
        assert_eq!(
            accept("application/json;q=1, application/graphql-response+json;q=0.5").json,
            Some(JsonMedia::Legacy)
        );
        assert_eq!(
            accept("application/json;q=0.5, application/graphql-response+json;q=0.9").json,
            Some(JsonMedia::GraphQLResponse)
        );
    }

    #[test]
    fn rejecting_legacy_json_keeps_the_modern_type_from_a_wildcard() {
        assert_eq!(
            accept("application/json;q=0, */*;q=1").json,
            Some(JsonMedia::GraphQLResponse)
        );
    }

    #[test]
    fn unsupported_types_are_not_acceptable() {
        assert_eq!(accept("text/plain").json, None);
        assert_eq!(
            accept("application/json;q=0, application/graphql-response+json;q=0").json,
            None
        );
    }

    #[test]
    fn event_stream_and_html_are_detected() {
        let accept = accept("text/event-stream, text/html");
        assert!(accept.event_stream);
        assert!(accept.html);
        assert_eq!(accept.json, None);
    }
}
