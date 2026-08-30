//! The transport behind heklang's `http.*` builtins.
//!
//! Split out from the effect driver because the driver is no longer the only caller:
//! the host adapter performs one attempt and heklang runs the retry loop, so the two
//! meet here rather than through the effect runtime.

use std::time::Duration;

use ureq::Agent;
use ureq::typestate::{WithBody, WithoutBody};

/// A raw HTTP request: what the effect host hands the transport.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

/// A raw HTTP response.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// The transport behind the journaled `http.*` builtins. Returns the response for
/// any HTTP status (including 4xx and 5xx); only a transport-level failure is an
/// `Err`. Split out so tests substitute a deterministic stub.
pub trait HttpClient: Send + Sync {
    fn send(&self, request: &HttpRequest) -> anyhow::Result<HttpResponse>;
}

/// The production transport, a blocking `ureq` agent with connect and overall
/// timeouts so a hung request cannot stall shutdown.
pub struct UreqClient {
    agent: Agent,
}

impl UreqClient {
    pub fn new() -> UreqClient {
        let config = Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_global(Some(Duration::from_secs(30)))
            // 4xx/5xx come back as responses to inspect, not transport errors.
            .http_status_as_error(false)
            .build();
        UreqClient {
            agent: Agent::new_with_config(config),
        }
    }

    fn without_body(
        &self,
        mut builder: ureq::RequestBuilder<WithoutBody>,
        request: &HttpRequest,
    ) -> anyhow::Result<HttpResponse> {
        for (key, value) in &request.headers {
            builder = builder.header(key.as_str(), value.as_str());
        }
        let response = builder
            .call()
            .map_err(|err| anyhow::anyhow!("http transport error: {err}"))?;
        read_response(response)
    }

    fn with_body(
        &self,
        mut builder: ureq::RequestBuilder<WithBody>,
        request: &HttpRequest,
    ) -> anyhow::Result<HttpResponse> {
        for (key, value) in &request.headers {
            builder = builder.header(key.as_str(), value.as_str());
        }
        let body = request.body.clone().unwrap_or_default();
        let response = builder
            .send(&body[..])
            .map_err(|err| anyhow::anyhow!("http transport error: {err}"))?;
        read_response(response)
    }
}

impl Default for UreqClient {
    fn default() -> UreqClient {
        UreqClient::new()
    }
}

impl HttpClient for UreqClient {
    fn send(&self, request: &HttpRequest) -> anyhow::Result<HttpResponse> {
        match request.method.as_str() {
            "GET" => self.without_body(self.agent.get(&request.url), request),
            "DELETE" => self.without_body(self.agent.delete(&request.url), request),
            "POST" => self.with_body(self.agent.post(&request.url), request),
            "PUT" => self.with_body(self.agent.put(&request.url), request),
            "PATCH" => self.with_body(self.agent.patch(&request.url), request),
            other => anyhow::bail!("unsupported http method `{other}`"),
        }
    }
}

fn read_response(mut response: ureq::http::Response<ureq::Body>) -> anyhow::Result<HttpResponse> {
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect();
    let body = response
        .body_mut()
        .read_to_vec()
        .map_err(|err| anyhow::anyhow!("reading http response body: {err}"))?;
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}
