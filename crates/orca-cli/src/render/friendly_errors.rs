//! `FriendlyErrors`: an error from the agent loop as one readable line plus an optional hint.
//!
//! C# unwraps exception types (`SocketException`, `HttpRequestException` with a status, the
//! retry `AggregateException`). In Rust the turn error is an
//! [`orca_core::agent::AgentError`] wrapping a [`ChatError`]; the REPL may only have its
//! Display text, so the classification works on the text ([`describe_message`]), and
//! [`describe`] first spells out the typed error's whole source chain (the socket reason sits
//! in reqwest's inner errors, not in its top-level message).

use std::sync::LazyLock;

use orca_core::agent::AgentError;
use orca_core::client::ChatError;
use regex::Regex;

const NETWORK_HINT: &str = "Check the server is running and the baseUrl in /config is right — /doctor runs a connection check.";

static HTTP_STATUS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^HTTP (\d{3}) from ").expect("static regex"));
static TRANSPORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)^request to \S+ failed: (.*)$").expect("static regex"));

/// `Describe` for the typed turn error: (message, hint).
pub fn describe(err: &AgentError, endpoint: &str) -> (String, Option<String>) {
    match err {
        AgentError::Chat(chat) => describe_chat(chat, endpoint),
    }
}

/// `Describe` for a [`ChatError`]: (message, hint).
pub fn describe_chat(err: &ChatError, endpoint: &str) -> (String, Option<String>) {
    describe_message(&chain_text(err), endpoint)
}

/// The error's message followed by each source's (": "-joined, repeats left out).
fn chain_text(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(s) = source {
        let m = s.to_string();
        if !m.is_empty() && !text.contains(&m) {
            text.push_str(": ");
            text.push_str(&m);
        }
        source = s.source();
    }
    text
}

/// `Describe` from an error's text (the Display of a `ChatError` / `AgentError`, or an
/// anyhow chain printed with `{:#}`): (message, hint).
pub fn describe_message(message: &str, endpoint: &str) -> (String, Option<String>) {
    let place = if endpoint.trim().is_empty() {
        "the model server"
    } else {
        endpoint
    };

    if let Some(detail) = TRANSPORT.captures(message).map(|c| c[1].to_string())
        && let Some(reason) = network_reason(&detail)
    {
        return (
            format!("Can't reach {place}: {reason}."),
            Some(NETWORK_HINT.to_string()),
        );
    }

    if let Some(code) = HTTP_STATUS
        .captures(message)
        .and_then(|c| c[1].parse::<u16>().ok())
    {
        let hint = match code {
            401 | 403 => Some("The server rejected the API key — check apiKey in /config."),
            404 => Some(
                "The endpoint or model wasn't found — check baseUrl (usually ends in /v1) and the model name (/model).",
            ),
            500.. => Some(
                "The server hit an internal error — its own log usually says why (out of memory, context too long).",
            ),
            _ => None,
        };
        return (
            format!("{place} returned HTTP {code} ({}).", status_name(code)),
            hint.map(str::to_string),
        );
    }

    (message.trim().to_string(), None)
}

/// The socket-level reason of a failed request (C# `SocketReason`), or the innermost
/// message when the request failed for a connection reason without a known code; `None`
/// when it was not a connection failure (e.g. a body that could not be read).
fn network_reason(detail: &str) -> Option<String> {
    let lower = detail.to_lowercase();
    let known = [
        (
            &[
                "connection refused",
                "actively refused",
                "os error 10061",
                "os error 111",
            ][..],
            "connection refused",
        ),
        (
            &[
                "timed out",
                "os error 10060",
                "os error 110",
                "operation timed out",
            ][..],
            "connection timed out",
        ),
        (
            &[
                "dns error",
                "failed to lookup address",
                "no such host",
                "name or service not known",
                "nodename nor servname",
            ][..],
            "host not found",
        ),
        (
            &[
                "host unreachable",
                "network unreachable",
                "network is unreachable",
                "no route to host",
            ][..],
            "host unreachable",
        ),
        (
            &[
                "connection reset",
                "forcibly closed",
                "os error 10054",
                "os error 104",
            ][..],
            "connection reset by the server",
        ),
    ];
    for (needles, reason) in known {
        if needles.iter().any(|n| lower.contains(n)) {
            return Some(reason.to_string());
        }
    }
    if lower.contains("error sending request") || lower.contains("connect") {
        // The innermost part of the chain, without a trailing period.
        let inner = detail.rsplit(": ").next().unwrap_or(detail).trim();
        return Some(inner.trim_end_matches('.').to_string());
    }
    None
}

/// .NET `HttpStatusCode.ToString()` for the common codes (the number otherwise).
fn status_name(code: u16) -> String {
    let name = match code {
        400 => "BadRequest",
        401 => "Unauthorized",
        402 => "PaymentRequired",
        403 => "Forbidden",
        404 => "NotFound",
        405 => "MethodNotAllowed",
        408 => "RequestTimeout",
        409 => "Conflict",
        413 => "RequestEntityTooLarge",
        415 => "UnsupportedMediaType",
        422 => "UnprocessableEntity",
        429 => "TooManyRequests",
        500 => "InternalServerError",
        501 => "NotImplemented",
        502 => "BadGateway",
        503 => "ServiceUnavailable",
        504 => "GatewayTimeout",
        _ => return code.to_string(),
    };
    name.to_string()
}

#[cfg(test)]
mod tests {
    //! Ported from FriendlyErrorsTests (UiPolishTests.cs). Rust has no retry
    //! `AggregateException`, so there is no "(tried N times)" suffix and no dedup of repeated
    //! inner messages; the cases keep their intent on the Rust error texts.
    use super::*;

    #[test]
    fn retry_aggregate_of_socket_errors_becomes_one_line_with_hint() {
        let msg = "request to http://gpu-host:8010/v1/chat/completions failed: error sending request for url (http://gpu-host:8010/v1/chat/completions): client error (Connect): tcp connect error: A connection attempt failed because the connected party did not properly respond after a period of time (os error 10060)";
        let (m, hint) = describe_message(msg, "http://gpu-host:8010/v1");
        assert_eq!(
            m,
            "Can't reach http://gpu-host:8010/v1: connection timed out."
        );
        assert!(hint.unwrap().contains("/doctor"));
    }

    #[test]
    fn http_status_is_named_with_a_hint() {
        let err = ChatError::Http {
            status: 404,
            url: "http://x/v1/chat/completions".into(),
            body: "not found".into(),
        };
        let (m, hint) = describe_chat(&err, "http://x/v1");
        assert!(m.contains("HTTP 404"));
        assert_eq!(m, "http://x/v1 returned HTTP 404 (NotFound).");
        assert!(hint.is_some());
    }

    #[test]
    fn other_errors_keep_their_message() {
        assert_eq!(describe_message("boom", "").0, "boom");
        let err = AgentError::Chat(ChatError::Protocol("bad json".into()));
        assert_eq!(
            describe(&err, "").0,
            "invalid response from the server: bad json"
        );
    }

    #[test]
    fn refused_connection_and_default_place() {
        let (m, _) = describe_message(
            "request to http://localhost:1234/v1/chat/completions failed: error sending request: tcp connect error: No connection could be made because the target machine actively refused it. (os error 10061)",
            "",
        );
        assert_eq!(m, "Can't reach the model server: connection refused.");
    }

    #[test]
    fn server_errors_and_auth_get_hints() {
        let (m, hint) = describe_message("HTTP 500 from http://x/v1: oom", "http://x/v1");
        assert_eq!(m, "http://x/v1 returned HTTP 500 (InternalServerError).");
        assert!(hint.unwrap().contains("internal error"));
        let (_, hint) = describe_message("HTTP 401 from http://x/v1: no", "http://x/v1");
        assert!(hint.unwrap().contains("apiKey"));
        let (_, hint) = describe_message("HTTP 418 from http://x/v1: tea", "http://x/v1");
        assert!(hint.is_none());
    }
}
