//! The one place that does HTTP. Behind a trait so the Riot and Telegram clients
//! are tested against fakes, offline.

use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    /// Only the headers anything reads (lower-cased names).
    pub retry_after: Option<String>,
    pub rate_limit_type: Option<String>,
    pub body: Vec<u8>,
}

/// A transport failure: timeout, reset, DNS, TLS. No HTTP status was received.
#[derive(Debug, Clone)]
pub struct TransportError(pub String);

pub trait Http: Send + Sync {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response, TransportError>;
    fn post_json(&self, url: &str, body: &[u8]) -> Result<Response, TransportError>;
}

/// ureq-backed transport: rustls, connection reuse, proxy from the environment.
pub struct UreqHttp {
    agent: ureq::Agent,
}

/// Bodies above this are refused: a match is ~30 KB, an update batch smaller.
const MAX_BODY: u64 = 4 * 1024 * 1024;

impl UreqHttp {
    pub fn new(timeout: Duration) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .max_idle_connections(4)
            .max_idle_connections_per_host(2)
            .user_agent("lolwatch")
            .build()
            .into();
        Self { agent }
    }

    fn finish(result: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<Response, TransportError> {
        let mut response = result.map_err(|e| TransportError(e.to_string()))?;
        let header = |name: &str| response.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let retry_after = header("retry-after");
        let rate_limit_type = header("x-rate-limit-type");
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY)
            .read_to_vec()
            .map_err(|e| TransportError(e.to_string()))?;
        Ok(Response { status, retry_after, rate_limit_type, body })
    }
}

impl Http for UreqHttp {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response, TransportError> {
        let mut request = self.agent.get(url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        Self::finish(request.call())
    }

    fn post_json(&self, url: &str, body: &[u8]) -> Result<Response, TransportError> {
        Self::finish(self.agent.post(url).header("Content-Type", "application/json").send(body))
    }
}

/// Percent-encode a URL path segment (everything but unreserved characters), like
/// Python's `quote(s, safe="")`.
pub fn encode_segment(segment: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.extend(['%', HEX[(byte >> 4) as usize] as char, HEX[(byte & 15) as usize] as char]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::encode_segment;

    #[test]
    fn segments_are_fully_encoded() {
        assert_eq!(encode_segment("Al ice"), "Al%20ice");
        assert_eq!(encode_segment("a/b#c?"), "a%2Fb%23c%3F");
        assert_eq!(encode_segment("Павел"), "%D0%9F%D0%B0%D0%B2%D0%B5%D0%BB");
        assert_eq!(encode_segment("x-y_z.~"), "x-y_z.~");
    }
}
