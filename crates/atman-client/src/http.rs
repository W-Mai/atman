use std::fmt;

use atman_proto::{JsonRpcRequest, JsonRpcResponse};
use futures::future::BoxFuture;
use reqwest::Url;

use crate::{RpcTransport, TransportError};

pub struct HttpTransport {
    client: reqwest::Client,
    rpc_url: Url,
    bearer_token: String,
}

impl HttpTransport {
    pub fn new(base_url: &str, bearer_token: impl Into<String>) -> Result<Self, TransportError> {
        let mut rpc_url = Url::parse(base_url)
            .map_err(|error| TransportError::InvalidEndpoint(error.to_string()))?;
        let path = rpc_url.path().trim_end_matches('/');
        rpc_url.set_path(&format!("{path}/rpc"));
        Ok(Self {
            client: reqwest::Client::new(),
            rpc_url,
            bearer_token: bearer_token.into(),
        })
    }

    pub fn rpc_url(&self) -> &Url {
        &self.rpc_url
    }
}

impl fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpTransport")
            .field("rpc_url", &self.rpc_url)
            .field("bearer_token", &"[redacted]")
            .finish_non_exhaustive()
    }
}

impl RpcTransport for HttpTransport {
    fn send(
        &self,
        request: JsonRpcRequest,
    ) -> BoxFuture<'_, Result<JsonRpcResponse, TransportError>> {
        Box::pin(async move {
            let response = self
                .client
                .post(self.rpc_url.clone())
                .bearer_auth(&self.bearer_token)
                .json(&request)
                .send()
                .await?;
            let status = response.status();
            if !status.is_success() {
                let mut body = response.text().await?;
                body.truncate(4_096);
                return Err(TransportError::HttpStatus {
                    status: status.as_u16(),
                    body,
                });
            }
            Ok(response.json().await?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_is_normalized_to_rpc_endpoint() {
        let transport = HttpTransport::new("http://127.0.0.1:7777/api/", "secret").unwrap();
        assert_eq!(
            transport.rpc_url().as_str(),
            "http://127.0.0.1:7777/api/rpc"
        );
        assert!(!format!("{transport:?}").contains("secret"));
    }
}
