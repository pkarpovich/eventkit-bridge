use std::time::Duration;

use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig};
use url::Url;

use super::jwks::{BODY_CAP, FetchError, JwksSource};

const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Fetches the JWK set from the provider's `jwks_url` over HTTP, trusting the OS certificate store.
pub struct HttpJwksSource {
    agent: Agent,
    url: Url,
}

impl HttpJwksSource {
    /// A source for `url` with a 10 s deadline per fetch.
    pub fn new(url: Url) -> Self {
        Self::with_timeout(url, FETCH_TIMEOUT)
    }

    fn with_timeout(url: Url, timeout: Duration) -> Self {
        let tls = TlsConfig::builder()
            .root_certs(RootCerts::PlatformVerifier)
            .build();
        let config = Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(true)
            .proxy(None)
            .tls_config(tls)
            .build();
        Self {
            agent: config.into(),
            url,
        }
    }
}

impl JwksSource for HttpJwksSource {
    fn fetch(&self) -> Result<Vec<u8>, FetchError> {
        let mut response = self.agent.get(self.url.as_str()).call().map_err(kind)?;
        response
            .body_mut()
            .with_config()
            .limit(BODY_CAP + 1)
            .read_to_vec()
            .map_err(kind)
    }
}

fn kind(err: ureq::Error) -> FetchError {
    match err {
        ureq::Error::StatusCode(code) => FetchError::Status(code),
        ureq::Error::Timeout(_) => FetchError::Timeout,
        ureq::Error::BodyExceedsLimit(_) => FetchError::TooLarge,
        _ => FetchError::Transport,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    const JWKS: &str = r#"{"keys":[]}"#;

    async fn serve_once(response: Vec<u8>) -> Url {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            while !request.ends_with(b"\r\n\r\n") {
                let read = stream.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            stream.write_all(&response).await.ok();
            stream.shutdown().await.ok();
        });
        Url::parse(&format!("http://{addr}/jwks.json")).unwrap()
    }

    fn response(status: &str, body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    async fn fetch(source: HttpJwksSource) -> Result<Vec<u8>, FetchError> {
        let source = Arc::new(source);
        tokio::task::spawn_blocking(move || source.fetch())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn ok_body_is_returned() {
        let url = serve_once(response("200 OK", JWKS.as_bytes())).await;

        let body = fetch(HttpJwksSource::new(url)).await.unwrap();

        assert_eq!(body, JWKS.as_bytes());
    }

    #[tokio::test]
    async fn server_error_is_its_status() {
        let url = serve_once(response("500 Internal Server Error", b"oops")).await;

        let result = fetch(HttpJwksSource::new(url)).await;

        assert_eq!(result, Err(FetchError::Status(500)));
    }

    #[tokio::test]
    async fn not_found_is_its_status() {
        let url = serve_once(response("404 Not Found", b"")).await;

        let result = fetch(HttpJwksSource::new(url)).await;

        assert_eq!(result, Err(FetchError::Status(404)));
    }

    #[tokio::test]
    async fn body_over_64_kib_is_too_large() {
        let body = vec![b' '; 64 * 1024 + 1];
        let url = serve_once(response("200 OK", &body)).await;

        let result = fetch(HttpJwksSource::new(url)).await;

        assert_eq!(result, Err(FetchError::TooLarge));
    }

    #[tokio::test]
    async fn body_of_exactly_64_kib_is_accepted() {
        let body = vec![b' '; 64 * 1024];
        let url = serve_once(response("200 OK", &body)).await;

        let fetched = fetch(HttpJwksSource::new(url)).await.unwrap();

        assert_eq!(fetched.len(), 64 * 1024);
    }

    #[tokio::test]
    async fn silent_server_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(stream);
        });
        let url = Url::parse(&format!("http://{addr}/jwks.json")).unwrap();

        let result = fetch(HttpJwksSource::with_timeout(
            url,
            Duration::from_millis(300),
        ))
        .await;

        assert_eq!(result, Err(FetchError::Timeout));
        server.abort();
    }

    #[tokio::test]
    async fn refused_connection_is_a_transport_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let url = Url::parse(&format!("http://{addr}/jwks.json")).unwrap();

        let result = fetch(HttpJwksSource::new(url)).await;

        assert_eq!(result, Err(FetchError::Transport));
    }
}
