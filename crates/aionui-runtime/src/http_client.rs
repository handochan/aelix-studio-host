use std::time::Duration;

const DEFAULT_RUNTIME_USER_AGENT: &str = concat!("aelix-studio-host/", env!("CARGO_PKG_VERSION"));

pub fn build_http_client(connect_timeout: Duration, timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(timeout)
        .user_agent(DEFAULT_RUNTIME_USER_AGENT)
        .build()
        .map_err(|error| format!("build http client: {error}"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::build_http_client;

    #[test]
    fn build_http_client_succeeds_with_runtime_defaults() {
        let _client = build_http_client(Duration::from_secs(1), Duration::from_secs(1)).expect("client");
    }

    #[tokio::test]
    async fn runtime_requests_send_the_host_user_agent() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 1024];
                let count = stream.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buffer[..count]);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8(request).unwrap()
        });
        let client = build_http_client(Duration::from_secs(5), Duration::from_secs(5)).unwrap();
        assert_eq!(
            client.get(format!("http://{address}/")).send().await.unwrap().status(),
            200
        );
        let request = server.await.unwrap().to_ascii_lowercase();
        assert!(request.contains(concat!(
            "\r\nuser-agent: aelix-studio-host/",
            env!("CARGO_PKG_VERSION"),
            "\r\n"
        )));
    }
}
