//! `Accept` header negotiation.
//!
//! GraphQL-over-HTTP defines two JSON media types: `application/graphql-response+json` for
//! responses and `application/json` for requests. A server must support the former; the latter is
//! kept for clients written before the specification existed ("legacy clients" in the
//! specification's terms).
//!
//! `text/html` (GraphiQL) and `text/event-stream` (subscriptions) are extensions this crate
//! offers on top of the specification, so they are only selected when a client explicitly asks
//! for them.
//!
//! The parser only needs quality values, so it intentionally ignores every other media-range
//! parameter.

use ::http::{HeaderMap, header};

/// Which JSON media type a response should use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JsonMedia {
    /// `application/json`, used for successful responses to a legacy client.
    Json,
    /// `application/graphql-response+json`, the media type from the specification.
    GraphQLResponse,
}

impl JsonMedia {
    pub(crate) const fn content_type(self) -> &'static str {
        match self {
            Self::Json => "application/json; charset=utf-8",
            Self::GraphQLResponse => "application/graphql-response+json; charset=utf-8",
        }
    }
}

/// The negotiated capabilities of one request.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Accept {
    /// The preferred JSON media type, when JSON is acceptable.
    pub(crate) json: Option<JsonMedia>,
    /// The quality of the best acceptable JSON media type. Zero when JSON is not acceptable.
    pub(crate) json_quality: f32,
    /// Whether `text/event-stream` is acceptable.
    pub(crate) event_stream: bool,
    /// The quality of `text/html`, when it is acceptable.
    pub(crate) html_quality: f32,
    /// Whether the request listed at least one media type this server can produce.
    pub(crate) acceptable: bool,
}

impl Accept {
    /// Whether HTML is strictly preferred over every JSON media type.
    ///
    /// A tie keeps JSON: a browser navigation sends only `text/html`, so an API client that
    /// explicitly lists both is asking for the machine-readable representation.
    pub(crate) fn prefers_html(self) -> bool {
        self.html_quality > 0.0 && self.html_quality > self.json_quality
    }

    /// The content type for a JSON response with `status`.
    ///
    /// The specification recommends that a legacy client (one that only accepts
    /// `application/json`) still receives the full range of status codes, but that successful
    /// responses use the legacy media type. Responses with a non-2xx status keep
    /// `application/graphql-response+json`, which is what makes the status code meaningful.
    pub(crate) fn json_content_type(self, status: ::http::StatusCode) -> &'static str {
        match (self.json, status.is_success()) {
            (Some(JsonMedia::Json), true) => JsonMedia::Json.content_type(),
            _ => JsonMedia::GraphQLResponse.content_type(),
        }
    }
}

/// Parse `Accept` headers into the capabilities this server understands.
pub(crate) fn parse(headers: &HeaderMap) -> Accept {
    let mut values = headers.get_all(header::ACCEPT).iter().peekable();
    if values.peek().is_none() {
        // The specification permits any representation when `Accept` is absent. This server
        // chooses the specification's media type.
        return Accept {
            json: Some(JsonMedia::GraphQLResponse),
            json_quality: 1.0,
            event_stream: false,
            html_quality: 0.0,
            acceptable: true,
        };
    }

    let mut graphql_q: Option<f32> = None;
    let mut json_q: Option<f32> = None;
    let mut application_q: Option<f32> = None;
    let mut any_q: Option<f32> = None;
    let mut event_q: Option<f32> = None;
    let mut html_q: Option<f32> = None;
    let mut text_q: Option<f32> = None;

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

            let slot = match range.as_str() {
                "application/graphql-response+json" => &mut graphql_q,
                "application/json" => &mut json_q,
                "application/*" => &mut application_q,
                "*/*" => &mut any_q,
                "text/event-stream" => &mut event_q,
                "text/html" => &mut html_q,
                "text/*" => &mut text_q,
                _ => continue,
            };
            *slot = Some(slot.map_or(quality, |current| current.max(quality)));
        }
    }

    // A specific range wins over a wildcard, including when its quality is zero (an explicit
    // rejection). `application/*` and `*/*` both cover the two JSON types; `text/*` covers the
    // two text extensions.
    let application_q = application_q.or(any_q);
    let graphql_quality = graphql_q.or(application_q).unwrap_or(0.0);
    let json_quality = json_q.or(application_q).unwrap_or(0.0);
    let event_stream = event_q.or(text_q).unwrap_or(0.0) > 0.0;
    let html_quality = html_q.or(text_q).unwrap_or(0.0);

    let json = if graphql_quality > 0.0 && graphql_quality >= json_quality {
        Some(JsonMedia::GraphQLResponse)
    } else if json_quality > 0.0 {
        Some(JsonMedia::Json)
    } else {
        None
    };
    let json_quality = json.map_or(0.0, |_| graphql_quality.max(json_quality));

    Accept {
        json,
        json_quality,
        event_stream,
        html_quality,
        acceptable: json.is_some() || event_stream || html_quality > 0.0,
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
    fn missing_header_defaults_to_the_specification_type() {
        let accept = parse(&HeaderMap::new());
        assert_eq!(accept.json, Some(JsonMedia::GraphQLResponse));
        assert!(!accept.event_stream);
        assert!(!accept.prefers_html());
        assert!(accept.acceptable);
    }

    #[test]
    fn wildcard_selects_the_specification_type() {
        assert_eq!(accept("*/*").json, Some(JsonMedia::GraphQLResponse));
        assert!(!accept("*/*").prefers_html());
    }

    #[test]
    fn application_json_is_the_legacy_fallback() {
        assert_eq!(accept("application/json").json, Some(JsonMedia::Json));
        assert_eq!(
            accept("application/graphql-response+json, application/json").json,
            Some(JsonMedia::GraphQLResponse)
        );
    }

    #[test]
    fn quality_values_select_the_higher_type() {
        assert_eq!(
            accept("application/json;q=1, application/graphql-response+json;q=0.5").json,
            Some(JsonMedia::Json)
        );
        assert_eq!(
            accept("application/json;q=0.5, application/graphql-response+json;q=0.9").json,
            Some(JsonMedia::GraphQLResponse)
        );
    }

    #[test]
    fn explicit_rejection_beats_a_wildcard() {
        assert_eq!(
            accept("application/json;q=0, */*;q=1").json,
            Some(JsonMedia::GraphQLResponse)
        );
        assert_eq!(accept("application/*;q=0, */*;q=1").json, None);
    }

    #[test]
    fn unsupported_types_are_not_acceptable() {
        assert!(!accept("text/plain").acceptable);
        assert!(!accept("application/json;q=0, application/graphql-response+json;q=0").acceptable);
    }

    #[test]
    fn event_stream_and_html_are_detected() {
        let accept = accept("text/event-stream, text/html");
        assert!(accept.event_stream);
        assert!(accept.prefers_html());
        assert_eq!(accept.json, None);
        assert!(accept.acceptable);
    }

    #[test]
    fn json_is_preferred_over_html_on_a_tie() {
        let accept = accept("text/html, application/graphql-response+json");
        assert_eq!(accept.json, Some(JsonMedia::GraphQLResponse));
        assert!(!accept.prefers_html());
    }

    #[test]
    fn legacy_successes_use_application_json() {
        let accept = accept("application/json");
        assert_eq!(
            accept.json_content_type(::http::StatusCode::OK),
            "application/json; charset=utf-8"
        );
        assert_eq!(
            accept.json_content_type(::http::StatusCode::NOT_FOUND),
            "application/graphql-response+json; charset=utf-8"
        );
    }
}
