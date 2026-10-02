use crate::{
    backend::{cancelled, Error},
    config::Tuya,
};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    sync::atomic::AtomicBool,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const RESPONSE_LIMIT: u64 = 256 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    client_id: String,
    client_secret: String,
}

impl Credentials {
    fn load(path: &std::path::Path) -> Result<Self, Error> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .map_err(|_| {
                Error::Config(
                    "Cannot open credentials file; use a private regular file, not a symlink"
                        .into(),
                )
            })?;
        let m = file
            .metadata()
            .map_err(|_| Error::Config("Cannot inspect credentials file".into()))?;
        if !m.is_file()
            || m.uid() != unsafe { libc::geteuid() }
            || m.mode() & 0o777 != 0o600
            || m.nlink() != 1
        {
            return Err(Error::Config("Credentials must be owned by this user, mode 0600, regular, and not hard-linked; do not use the Nix store".into()));
        }
        let mut text = String::new();
        file.take(16385)
            .read_to_string(&mut text)
            .map_err(|_| Error::Config("Cannot read credentials".into()))?;
        if text.len() > 16384 {
            return Err(Error::Config("Credentials file is too large".into()));
        }
        let credentials: Self = toml::from_str(&text).map_err(|_| {
            Error::Config("Invalid credentials TOML; expected client_id and client_secret".into())
        })?;
        if [&credentials.client_id, &credentials.client_secret]
            .iter()
            .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_graphic()))
        {
            return Err(Error::Config(
                "Credentials must be nonempty ASCII values without whitespace".into(),
            ));
        }
        Ok(credentials)
    }
}

// No Debug implementation: requests include secrets and must not reach diagnostics.
pub(crate) struct Request {
    method: &'static str,
    path: String,
    body: Vec<u8>,
    headers: Vec<(&'static str, String)>,
}

pub(crate) trait Transport {
    fn send(
        &mut self,
        request: Request,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<(u16, Vec<u8>), Error>;
}

pub(crate) struct Http {
    client: reqwest::Client,
    endpoint: String,
    runtime: Option<tokio::runtime::Runtime>,
}
impl Http {
    fn new(endpoint: &str, timeout: Duration) -> Result<Self, Error> {
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(timeout)
            .connect_timeout(timeout)
            .build()
            .map_err(|_| Error::Cloud("Cannot initialize secure HTTP client".into()))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| Error::Cloud("Cannot initialize HTTP runtime".into()))?;
        Ok(Self {
            client,
            endpoint: endpoint.trim_end_matches('/').into(),
            runtime: Some(runtime),
        })
    }
}
impl Drop for Http {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            // Async requests are dropped on shutdown. A blocking DNS lookup may finish
            // separately, but cannot keep Quit waiting or execute a power request.
            runtime.shutdown_timeout(Duration::ZERO);
        }
    }
}
impl Transport for Http {
    fn send(
        &mut self,
        request: Request,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<(u16, Vec<u8>), Error> {
        cancelled(cancel)?;
        if timeout.is_zero() {
            return Err(Error::Cloud("Tuya request deadline expired".into()));
        }
        let deadline = Instant::now() + timeout;
        let method =
            reqwest::Method::from_bytes(request.method.as_bytes()).expect("fixed HTTP method");
        let mut builder = self
            .client
            .request(method, format!("{}{}", self.endpoint, request.path))
            .timeout(timeout)
            .body(request.body)
            .header("Content-Type", "application/json");
        for (key, value) in request.headers {
            builder = builder.header(key, value);
        }
        let result = self.runtime.as_ref().expect("HTTP runtime is alive").block_on(async {
            // Dropping this branch cancels send/body reads, not just the synchronous wait.
            // No request task or thread is detached and commands are never resubmitted.
            tokio::select! {
                biased;
                _ = async {
                    while cancelled(cancel).is_ok() {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                } => Err(Error::Cancelled),
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    Err(Error::Cloud("Tuya request deadline expired".into()))
                }
                result = async {
                    cancelled(cancel)?;
                    let mut response = builder.send().await.map_err(|_| Error::Cloud("Tuya request failed or timed out; check network, regional endpoint and system clock".into()))?;
                    let status = response.status().as_u16();
                    if response.content_length().is_some_and(|length| length > RESPONSE_LIMIT) {
                        return Err(Error::Cloud("Tuya response exceeded safe size limit".into()));
                    }
                    let mut bytes = Vec::new();
                    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Cloud("Cannot read Tuya response".into()))? {
                        cancelled(cancel)?;
                        if Instant::now() >= deadline { return Err(Error::Cloud("Tuya request deadline expired".into())); }
                        if chunk.len() as u64 > RESPONSE_LIMIT - bytes.len() as u64 {
                            return Err(Error::Cloud("Tuya response exceeded safe size limit".into()));
                        }
                        bytes.extend_from_slice(&chunk);
                    }
                    cancelled(cancel)?;
                    if Instant::now() >= deadline { return Err(Error::Cloud("Tuya request deadline expired".into())); }
                    Ok((status, bytes))
                } => result,
            }
        });
        cancelled(cancel)?;
        result
    }
}

fn canonical_path(path: &str) -> String {
    match path.split_once('?') {
        None => path.into(),
        Some((base, query)) => {
            let mut pairs: Vec<_> = query.split('&').filter(|p| !p.is_empty()).collect();
            pairs.sort_unstable();
            if pairs.is_empty() {
                base.into()
            } else {
                format!("{base}?{}", pairs.join("&"))
            }
        }
    }
}

fn signature(
    secret: &str,
    prefix: &str,
    method: &str,
    path: &str,
    body: &[u8],
    signed_headers: &str,
) -> String {
    let digest = hex::encode(Sha256::digest(body));
    let text = format!(
        "{prefix}{method}\n{digest}\n{signed_headers}\n{}",
        canonical_path(path)
    );
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts arbitrary key lengths");
    mac.update(text.as_bytes());
    hex::encode_upper(mac.finalize().into_bytes())
}

enum ApiFailure {
    Expired,
    Other(Error),
}

fn parse_response(status: u16, body: &[u8]) -> Result<Value, ApiFailure> {
    let fail = |message: &str| ApiFailure::Other(Error::Cloud(message.into()));
    if status == 429 {
        return Err(fail("Tuya rate limit reached; wait before trying again"));
    }
    if !(200..300).contains(&status) {
        return Err(fail(
            "Tuya HTTP failure; check regional endpoint and service availability",
        ));
    }
    let value: Value = serde_json::from_slice(body).map_err(|_| fail("Malformed Tuya response"))?;
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        let code = value
            .get("code")
            .map(|v| match v {
                Value::String(s) => s.clone(),
                _ => v.to_string(),
            })
            .unwrap_or_default();
        return Err(match code.as_str() {
            "1010" | "1011" => ApiFailure::Expired,
            "1004" | "1005" => fail("Tuya authentication failed; verify Access ID/Secret, region and system clock"),
            "1013" => fail("Tuya request timestamp rejected; synchronize the system clock"),
            "1106" | "1100" => fail("Tuya access denied; check device authorization, account linking and API permissions"),
            "2015" => fail("Tuya device is offline; check plug connectivity"),
            "28841002" => fail("Tuya service entitlement expired; renew the project API service"),
            _ => fail("Tuya rejected the request; check region, API entitlement, device authorization and rate limits"),
        });
    }
    value
        .get("result")
        .cloned()
        .filter(|v| !v.is_null())
        .ok_or_else(|| fail("Tuya success response has no result"))
}

pub(crate) trait Plug {
    fn validate(&mut self, timeout: Duration, cancel: &AtomicBool) -> Result<(), Error>;
    fn status(&mut self, timeout: Duration, cancel: &AtomicBool) -> Result<bool, Error>;
    fn command(&mut self, on: bool, timeout: Duration, cancel: &AtomicBool) -> Result<(), Error>;
}

pub(crate) struct Cloud<T> {
    transport: T,
    credentials: Credentials,
    config: Tuya,
    token: String,
}

impl Cloud<Http> {
    pub(crate) fn new(config: &Tuya, timeout: Duration) -> Result<Self, Error> {
        Ok(Self {
            transport: Http::new(&config.endpoint, timeout)?,
            credentials: Credentials::load(&config.credentials_file)?,
            config: config.clone(),
            token: String::new(),
        })
    }
}

impl<T: Transport> Cloud<T> {
    fn request(
        &mut self,
        method: &'static str,
        path: &str,
        body: Vec<u8>,
        token_request: bool,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<Value, ApiFailure> {
        cancelled(cancel).map_err(ApiFailure::Other)?;
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return Err(ApiFailure::Other(Error::Cloud(
                "Tuya request deadline expired".into(),
            )));
        }
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ApiFailure::Other(Error::Cloud("System clock is invalid".into())))?
            .as_millis()
            .to_string();
        let token = if token_request { "" } else { &self.token };
        let prefix = format!("{}{token}{timestamp}", self.credentials.client_id);
        let sign = signature(
            &self.credentials.client_secret,
            &prefix,
            method,
            path,
            &body,
            "",
        );
        let mut headers = vec![
            ("client_id", self.credentials.client_id.clone()),
            ("t", timestamp),
            ("sign_method", "HMAC-SHA256".into()),
            ("sign", sign),
        ];
        if !token_request {
            headers.push(("access_token", self.token.clone()));
        }
        let (status, bytes) = self
            .transport
            .send(
                Request {
                    method,
                    path: canonical_path(path),
                    body,
                    headers,
                },
                timeout,
                cancel,
            )
            .map_err(ApiFailure::Other)?;
        cancelled(cancel).map_err(ApiFailure::Other)?;
        parse_response(status, &bytes)
    }

    fn authenticate(&mut self, deadline: Instant, cancel: &AtomicBool) -> Result<(), Error> {
        let value = self
            .request(
                "GET",
                "/v1.0/token?grant_type=1",
                vec![],
                true,
                deadline,
                cancel,
            )
            .map_err(api_error)?;
        self.token = value
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() < 8192 && s.bytes().all(|b| b.is_ascii_graphic()))
            .ok_or_else(|| Error::Cloud("Tuya token response is invalid".into()))?
            .into();
        Ok(())
    }

    fn call(
        &mut self,
        method: &'static str,
        suffix: &str,
        body: Vec<u8>,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<Value, Error> {
        let deadline = Instant::now() + timeout;
        if self.token.is_empty() {
            self.authenticate(deadline, cancel)?;
        }
        let path = format!("/v1.0/devices/{}{suffix}", self.config.device_id);
        match self.request(method, &path, body.clone(), false, deadline, cancel) {
            // Only a safe read is retried; commands are never blindly resent.
            Err(ApiFailure::Expired) if method == "GET" => {
                self.authenticate(deadline, cancel)?;
                self.request(method, &path, body, false, deadline, cancel)
                    .map_err(api_error)
            }
            result => result.map_err(api_error),
        }
    }
}

fn api_error(error: ApiFailure) -> Error {
    match error {
        ApiFailure::Expired => {
            Error::Cloud("Tuya token expired; retry the operation to authenticate again".into())
        }
        ApiFailure::Other(e) => e,
    }
}

impl<T: Transport> Plug for Cloud<T> {
    fn validate(&mut self, timeout: Duration, cancel: &AtomicBool) -> Result<(), Error> {
        let result = self.call("GET", "/functions", vec![], timeout, cancel)?;
        let functions = result
            .get("functions")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Cloud("Tuya functions response is invalid".into()))?;
        let matches: Vec<_> = functions
            .iter()
            .filter(|v| v.get("code").and_then(Value::as_str) == Some(&self.config.switch_code))
            .collect();
        if matches.len() != 1
            || matches[0]
                .get("type")
                .and_then(Value::as_str)
                .is_none_or(|t| !t.eq_ignore_ascii_case("boolean"))
        {
            return Err(Error::Cloud("Configured switch code is not a supported boolean control; inspect the device functions".into()));
        }
        let details = self.call("GET", "", vec![], timeout, cancel)?;
        match details.get("online").and_then(Value::as_bool) {
            Some(true) => Ok(()),
            Some(false) => Err(Error::Cloud(
                "Tuya plug is offline; restore its network connection".into(),
            )),
            None => Err(Error::Cloud(
                "Tuya device response has no reliable online status".into(),
            )),
        }
    }

    fn status(&mut self, timeout: Duration, cancel: &AtomicBool) -> Result<bool, Error> {
        let result = self.call("GET", "/status", vec![], timeout, cancel)?;
        let statuses = result
            .as_array()
            .ok_or_else(|| Error::Cloud("Invalid Tuya switch status response".into()))?;
        let matches: Vec<_> = statuses
            .iter()
            .filter(|v| v.get("code").and_then(Value::as_str) == Some(&self.config.switch_code))
            .collect();
        if matches.len() != 1 {
            return Err(Error::Cloud(
                "Configured switch code missing or duplicated in device status".into(),
            ));
        }
        matches[0]
            .get("value")
            .and_then(Value::as_bool)
            .ok_or_else(|| Error::Cloud("Configured switch status is not boolean".into()))
    }

    fn command(&mut self, on: bool, timeout: Duration, cancel: &AtomicBool) -> Result<(), Error> {
        let body = serde_json::to_vec(
            &json!({"commands": [{"code": self.config.switch_code, "value": on}]}),
        )
        .expect("serializable command");
        let result = self.call("POST", "/commands", body, timeout, cancel)?;
        if result != Value::Bool(true) {
            return Err(Error::Cloud(
                "Tuya did not accept the switch command; power state is unconfirmed".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        io::Write,
        net::{TcpListener, TcpStream},
        os::unix::fs::{symlink, PermissionsExt},
        sync::{atomic::Ordering, mpsc, Arc, Mutex},
        thread,
    };

    struct Reply {
        header_delay: Duration,
        body_delay: Duration,
        body: Option<Vec<u8>>,
        status: u16,
        extra_headers: String,
        chunked: bool,
    }
    impl Reply {
        fn json(result: Value) -> Self {
            Self {
                header_delay: Duration::ZERO,
                body_delay: Duration::ZERO,
                body: Some(success(result).1),
                status: 200,
                extra_headers: String::new(),
                chunked: false,
            }
        }
        fn stalled(headers: bool) -> Self {
            Self {
                header_delay: if headers {
                    Duration::ZERO
                } else {
                    Duration::from_secs(86400)
                },
                body_delay: Duration::ZERO,
                body: None,
                status: 200,
                extra_headers: String::new(),
                chunked: false,
            }
        }
    }

    type RecordedRequests = Vec<(String, Vec<u8>)>;
    struct LocalServer {
        endpoint: String,
        requests: Arc<Mutex<RecordedRequests>>,
        stages: mpsc::Receiver<usize>,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }
    impl LocalServer {
        fn new(replies: Vec<Reply>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded = requests.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let stopped = stop.clone();
            let (stage_tx, stages) = mpsc::channel();
            let worker = thread::spawn(move || {
                let mut replies: VecDeque<_> = replies.into();
                while !stopped.load(Ordering::Relaxed) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(connection) => connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(error) => panic!("local accept failed: {error}"),
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_millis(20)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_millis(100)))
                        .unwrap();
                    let Some(request) = Self::read_request(&mut stream, &stopped) else {
                        continue;
                    };
                    let index = {
                        let mut requests = recorded.lock().unwrap();
                        requests.push(request);
                        requests.len()
                    };
                    let _ = stage_tx.send(index);
                    let Some(reply) = replies.pop_front() else {
                        continue;
                    };
                    if !Self::delay(reply.header_delay, &stopped) {
                        break;
                    }
                    let length = reply.body.as_ref().map_or(16, Vec::len);
                    let framing = if reply.chunked {
                        "Transfer-Encoding: chunked".into()
                    } else {
                        format!("Content-Length: {length}")
                    };
                    if write!(
                        stream,
                        "HTTP/1.1 {} Test\r\n{framing}\r\n{}Connection: close\r\n\r\n",
                        reply.status, reply.extra_headers
                    )
                    .is_err()
                    {
                        continue;
                    }
                    if !Self::delay(reply.body_delay, &stopped) {
                        break;
                    }
                    if let Some(body) = reply.body {
                        if reply.chunked {
                            for chunk in body.chunks(4096) {
                                if write!(stream, "{:x}\r\n", chunk.len()).is_err()
                                    || stream.write_all(chunk).is_err()
                                    || stream.write_all(b"\r\n").is_err()
                                {
                                    break;
                                }
                            }
                            let _ = stream.write_all(b"0\r\n\r\n");
                        } else {
                            let _ = stream.write_all(&body);
                        }
                    } else {
                        // Keep the body incomplete until cancellation closes the connection.
                        let mut byte = [0];
                        while !stopped.load(Ordering::Relaxed) {
                            match stream.read(&mut byte) {
                                Ok(0) => break,
                                Err(error)
                                    if matches!(
                                        error.kind(),
                                        std::io::ErrorKind::WouldBlock
                                            | std::io::ErrorKind::TimedOut
                                            | std::io::ErrorKind::Interrupted
                                    ) => {}
                                Err(_) => break,
                                Ok(_) => {}
                            }
                        }
                    }
                }
            });
            Self {
                endpoint,
                requests,
                stages,
                stop,
                worker: Some(worker),
            }
        }

        fn delay(duration: Duration, stop: &AtomicBool) -> bool {
            let deadline = Instant::now() + duration;
            while Instant::now() < deadline {
                if stop.load(Ordering::Relaxed) {
                    return false;
                }
                thread::sleep(
                    Duration::from_millis(5)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            !stop.load(Ordering::Relaxed)
        }

        fn read_request(stream: &mut TcpStream, stop: &AtomicBool) -> Option<(String, Vec<u8>)> {
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            let deadline = Instant::now() + Duration::from_secs(3);
            while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                match stream.read(&mut buffer) {
                    Ok(0) => return None,
                    Ok(length) => bytes.extend_from_slice(&buffer[..length]),
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::Interrupted
                        ) =>
                    {
                        continue
                    }
                    Err(_) => return None,
                }
                assert!(bytes.len() < 65536, "test request exceeded limit");
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&bytes[..end]).unwrap();
                    let length = headers
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                        .map_or(0, |(_, value)| value.trim().parse::<usize>().unwrap());
                    if bytes.len() >= end + 4 + length {
                        return Some((
                            headers.lines().next().unwrap().into(),
                            bytes[end + 4..end + 4 + length].to_vec(),
                        ));
                    }
                }
            }
            None
        }

        fn wait_for(&self, index: usize) {
            loop {
                let actual = self
                    .stages
                    .recv_timeout(Duration::from_secs(3))
                    .expect("local server did not receive request");
                if actual == index {
                    return;
                }
                assert!(actual < index);
            }
        }
    }
    impl Drop for LocalServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            self.worker.take().unwrap().join().unwrap();
        }
    }

    fn local_http(endpoint: &str) -> Http {
        // Only tests replace the HTTPS-only client/origin; production has no HTTP override.
        let mut http = Http::new("https://example.invalid", Duration::from_secs(86400)).unwrap();
        http.client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(86400))
            .build()
            .unwrap();
        http.endpoint = endpoint.into();
        http
    }

    fn local_request() -> Request {
        Request {
            method: "POST",
            path: "/v1.0/devices/fake/commands".into(),
            body: br#"{"commands":[{"code":"switch_1","value":true}]}"#.to_vec(),
            headers: vec![],
        }
    }

    #[test]
    fn http_cancellation_bounds_stalled_headers_and_body_with_day_long_timeout() {
        for headers in [false, true] {
            let server = LocalServer::new(vec![Reply::stalled(headers)]);
            let mut http = local_http(&server.endpoint);
            let cancel = Arc::new(AtomicBool::new(false));
            let cancelled = cancel.clone();
            let (tx, rx) = mpsc::channel();
            let worker = thread::spawn(move || {
                let result = http.send(local_request(), Duration::from_secs(86400), &cancelled);
                tx.send((http, result)).unwrap();
            });
            server.wait_for(1);
            // Ensure the response branch is already waiting, not only pre-cancelled.
            thread::sleep(Duration::from_millis(60));
            let begin = Instant::now();
            cancel.store(true, Ordering::Relaxed);
            let (mut http, result) = rx
                .recv_timeout(Duration::from_secs(1))
                .expect("HTTP ignored cancellation");
            assert!(matches!(result, Err(Error::Cancelled)));
            assert!(begin.elapsed() < Duration::from_secs(1));
            worker.join().unwrap();
            assert!(matches!(
                http.send(local_request(), Duration::from_secs(86400), &cancel),
                Err(Error::Cancelled)
            ));
            // Drive any connection cleanup tasks: no cancelled command may be resubmitted.
            http.runtime.as_ref().unwrap().block_on(async {
                tokio::time::sleep(Duration::from_millis(60)).await;
            });
            assert_eq!(server.requests.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn http_deadline_covers_headers_and_body_without_resetting() {
        for headers in [false, true] {
            let server = LocalServer::new(vec![Reply::stalled(headers)]);
            let mut http = local_http(&server.endpoint);
            let begin = Instant::now();
            assert!(matches!(
                http.send(
                    local_request(),
                    Duration::from_millis(100),
                    &AtomicBool::new(false)
                ),
                Err(Error::Cloud(_))
            ));
            assert!(begin.elapsed() < Duration::from_secs(1));
            assert_eq!(server.requests.lock().unwrap().len(), 1);
        }
        let mut reply = Reply::json(json!(true));
        reply.header_delay = Duration::from_millis(150);
        reply.body_delay = Duration::from_millis(150);
        let server = LocalServer::new(vec![reply]);
        let mut http = local_http(&server.endpoint);
        assert!(matches!(
            http.send(
                local_request(),
                Duration::from_millis(240),
                &AtomicBool::new(false)
            ),
            Err(Error::Cloud(_))
        ));
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn async_http_retains_size_limit_redaction_and_redirect_refusal() {
        let server = LocalServer::new(vec![Reply::json(json!(true))]);
        let mut http = local_http(&server.endpoint);
        let (status, bytes) = http
            .send(
                local_request(),
                Duration::from_secs(1),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(parse_response(status, &bytes).ok(), Some(Value::Bool(true)));
        for (body, chunked) in [
            (vec![b'x'; RESPONSE_LIMIT as usize + 1], false),
            (vec![b'x'; RESPONSE_LIMIT as usize + 1], true),
            (b"SECRET malformed response".to_vec(), false),
        ] {
            let server = LocalServer::new(vec![Reply {
                header_delay: Duration::ZERO,
                body_delay: Duration::ZERO,
                body: Some(body),
                status: 200,
                extra_headers: String::new(),
                chunked,
            }]);
            let mut http = local_http(&server.endpoint);
            let error = match http.send(
                local_request(),
                Duration::from_secs(1),
                &AtomicBool::new(false),
            ) {
                Ok((status, bytes)) => parse_response(status, &bytes).err().map(api_error).unwrap(),
                Err(error) => error,
            };
            assert!(!error.to_string().contains("SECRET"));
        }
        let target = LocalServer::new(vec![]);
        let mut redirect = Reply::json(json!(true));
        redirect.status = 302;
        redirect.extra_headers =
            format!("Location: {}/must-not-receive-secrets\r\n", target.endpoint);
        let server = LocalServer::new(vec![redirect]);
        let mut http = local_http(&server.endpoint);
        assert_eq!(
            http.send(
                local_request(),
                Duration::from_secs(1),
                &AtomicBool::new(false)
            )
            .unwrap()
            .0,
            302
        );
        assert!(target.requests.lock().unwrap().is_empty());
        // Production's HTTPS-only client must reject plaintext even at a local endpoint.
        let server = LocalServer::new(vec![]);
        let mut http = Http::new(&server.endpoint, Duration::from_secs(1)).unwrap();
        assert!(http
            .send(
                local_request(),
                Duration::from_secs(1),
                &AtomicBool::new(false)
            )
            .is_err());
        assert!(server.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn cancelled_http_workflow_releases_lock_and_never_sends_later_commands() {
        if crate::process::isolated_test(
            "cloud::tests::cancelled_http_workflow_releases_lock_and_never_sends_later_commands",
        ) {
            return;
        }
        use crate::{
            backend::{lock_at, workflow, Event, Operation, Runtime},
            config::{Config, Moonlight, Notifications, Startup},
            process::Output,
        };
        struct Host(Instant);
        impl Runtime for Host {
            fn now(&self) -> Duration {
                self.0.elapsed()
            }
            fn sleep(&mut self, _: Duration, _: &AtomicBool) -> Result<(), Error> {
                panic!("cancelled HTTP must stop workflow")
            }
            fn probe(&mut self, _: Duration, _: &AtomicBool) -> Result<Output, Error> {
                panic!("no probe after HTTP cancellation")
            }
            fn stream(&mut self, _: &AtomicBool, _: &dyn Fn(Event)) -> Result<(), Error> {
                panic!("no stream after HTTP cancellation")
            }
            fn notify(&mut self, _: &str) {}
        }
        for operation in [Operation::Start, Operation::PlugOff] {
            for command_started in [false, true] {
                let mut replies = vec![];
                if command_started {
                    replies.extend([
                        Reply::json(json!({"functions":[{"code":"switch_1","type":"Boolean"}]})),
                        Reply::json(json!({"online":true})),
                        Reply::json(
                            json!([{"code":"switch_1","value":operation == Operation::PlugOff}]),
                        ),
                    ]);
                }
                replies.push(Reply::stalled(true));
                let server = LocalServer::new(replies);
                let fixture = cloud(vec![]);
                let mut cloud = Cloud {
                    transport: local_http(&server.endpoint),
                    credentials: fixture.credentials,
                    config: fixture.config,
                    token: "synthetic-token".into(),
                };
                let config = Config {
                    tuya: cloud.config.clone(),
                    moonlight: Moonlight {
                        executable: "never-spawn".into(),
                        host: "fake".into(),
                        app: "Desktop".into(),
                        stream_args: vec![],
                    },
                    startup: Startup {
                        timeout_seconds: 180,
                        poll_interval_seconds: 3,
                        probe_timeout_seconds: 10,
                        http_timeout_seconds: 86400,
                    },
                    notifications: Notifications { enabled: false },
                };
                let directory = tempfile::tempdir().unwrap();
                std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                    .unwrap();
                let path = directory.path().to_owned();
                let cancel = Arc::new(AtomicBool::new(false));
                let cancelled = cancel.clone();
                let (tx, rx) = mpsc::channel();
                let worker = thread::spawn(move || {
                    let result = (|| {
                        let _lock = lock_at(&path, "operation")?;
                        workflow(
                            &config,
                            operation,
                            &cancelled,
                            &|_| {},
                            || operation == Operation::PlugOff,
                            &mut cloud,
                            &mut Host(Instant::now()),
                        )
                    })();
                    tx.send((cloud, result)).unwrap();
                });
                server.wait_for(if command_started { 4 } else { 1 });
                assert!(matches!(
                    lock_at(directory.path(), "operation"),
                    Err(Error::Busy)
                ));
                thread::sleep(Duration::from_millis(60));
                cancel.store(true, Ordering::Relaxed);
                let (cloud, result) = rx
                    .recv_timeout(Duration::from_secs(1))
                    .expect("workflow did not cancel boundedly");
                assert!(matches!(result, Err(Error::Cancelled)));
                worker.join().unwrap();
                drop(lock_at(directory.path(), "operation").unwrap());
                cloud.transport.runtime.as_ref().unwrap().block_on(async {
                    tokio::time::sleep(Duration::from_millis(60)).await;
                });
                let requests = server.requests.lock().unwrap();
                assert_eq!(requests.len(), if command_started { 4 } else { 1 });
                let commands: Vec<_> = requests
                    .iter()
                    .filter(|(line, _)| line.starts_with("POST "))
                    .collect();
                assert_eq!(commands.len(), usize::from(command_started));
                if command_started {
                    let body: Value = serde_json::from_slice(&commands[0].1).unwrap();
                    assert_eq!(body["commands"][0]["value"], operation == Operation::Start);
                }
            }
        }
    }

    #[test]
    fn current_tuya_public_signing_fixtures() {
        let id = "1KAD46OrT9HafiKdsXeg";
        let secret = "4OHBOnWOqaEC1mWXOpVL3yV50s0qGSRC";
        let t_nonce = "15889257780005138cc3a9033d69856923fd07b491173";
        let headers = "area_id:29a33e8796834b1efa6\ncall_id:8afdb70ab2ed11eb85290242ac130003\n";
        assert_eq!(
            signature(
                secret,
                &format!("{id}{t_nonce}"),
                "GET",
                "/v1.0/token?grant_type=1",
                b"",
                headers
            ),
            "9E48A3E93B302EEECC803C7241985D0A34EB944F40FB573C7B5C2A82158AF13E"
        );
        assert_eq!(
            signature(
                secret,
                &format!("{id}3f4eda2bdec17232f67c0b188af3eec1{t_nonce}"),
                "GET",
                "/v2.0/apps/schema/users?page_size=50&page_no=1",
                b"",
                headers
            ),
            "AE4481C692AA80B25F3A7E12C3A5FD9BBF6251539DD78E565A1A72A508A88784"
        );
        assert_eq!(canonical_path("/path?z=2&a=1"), "/path?a=1&z=2");
        assert_eq!(canonical_path("/path?"), "/path");
        assert_ne!(
            signature(secret, id, "POST", "/path", b"true", ""),
            signature(secret, id, "POST", "/path", b"false", "")
        );
    }

    struct FakeTransport {
        replies: VecDeque<(u16, Vec<u8>)>,
        requests: Vec<Request>,
    }
    impl Transport for FakeTransport {
        fn send(
            &mut self,
            request: Request,
            timeout: Duration,
            cancel: &AtomicBool,
        ) -> Result<(u16, Vec<u8>), Error> {
            cancelled(cancel)?;
            assert!(!timeout.is_zero());
            let header = |name| {
                request
                    .headers
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.as_str())
            };
            assert_eq!(header("client_id"), Some("synthetic-id"));
            assert_eq!(header("sign_method"), Some("HMAC-SHA256"));
            let timestamp = header("t").unwrap();
            assert!(timestamp.bytes().all(|b| b.is_ascii_digit()));
            let token = header("access_token").unwrap_or("");
            assert_eq!(token.is_empty(), request.path.starts_with("/v1.0/token?"));
            let expected = signature(
                "synthetic-secret",
                &format!("synthetic-id{token}{timestamp}"),
                request.method,
                &request.path,
                &request.body,
                "",
            );
            assert_eq!(header("sign"), Some(expected.as_str()));
            self.requests.push(request);
            Ok(self.replies.pop_front().expect("unexpected HTTP request"))
        }
    }
    fn success(result: Value) -> (u16, Vec<u8>) {
        (
            200,
            serde_json::to_vec(&json!({"success": true, "result": result})).unwrap(),
        )
    }
    fn cloud(replies: Vec<(u16, Vec<u8>)>) -> Cloud<FakeTransport> {
        Cloud {
            transport: FakeTransport {
                replies: replies.into(),
                requests: vec![],
            },
            credentials: Credentials {
                client_id: "synthetic-id".into(),
                client_secret: "synthetic-secret".into(),
            },
            config: Tuya {
                endpoint: "https://example.invalid".into(),
                device_id: "fake".into(),
                switch_code: "switch_1".into(),
                credentials_file: "/never-read".into(),
            },
            token: String::new(),
        }
    }
    fn token() -> (u16, Vec<u8>) {
        success(json!({"access_token": "synthetic-token"}))
    }
    fn status(on: bool) -> (u16, Vec<u8>) {
        success(json!([{"code": "switch_1", "value": on}]))
    }
    fn expired() -> (u16, Vec<u8>) {
        (
            200,
            br#"{"success":false,"code":1010,"msg":"SECRET MUST NOT LEAK"}"#.to_vec(),
        )
    }

    #[test]
    fn signed_transport_boolean_controls_status_and_exact_body() {
        let mut cloud = cloud(vec![
            token(),
            success(json!({"functions": [{"code": "switch_1", "type": "Boolean"}]})),
            success(json!({"online": true})),
            status(false),
            success(json!(true)),
            status(true),
        ]);
        let cancel = AtomicBool::new(false);
        cloud.validate(Duration::from_secs(1), &cancel).unwrap();
        assert!(!cloud.status(Duration::from_secs(1), &cancel).unwrap());
        cloud
            .command(true, Duration::from_secs(1), &cancel)
            .unwrap();
        assert!(cloud.status(Duration::from_secs(1), &cancel).unwrap());
        let request = &cloud.transport.requests[4];
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1.0/devices/fake/commands");
        assert_eq!(
            request.body,
            br#"{"commands":[{"code":"switch_1","value":true}]}"#
        );
        assert!(cloud.transport.replies.is_empty());
    }

    #[test]
    fn token_renewal_once_for_reads_never_commands() {
        let cancel = AtomicBool::new(false);
        let mut read = cloud(vec![token(), expired(), token(), status(true)]);
        assert!(read.status(Duration::from_secs(1), &cancel).unwrap());
        assert_eq!(read.transport.requests.len(), 4);
        let mut repeated = cloud(vec![token(), expired(), token(), expired()]);
        assert!(repeated.status(Duration::from_secs(1), &cancel).is_err());
        assert_eq!(repeated.transport.requests.len(), 4);
        let mut command = cloud(vec![token(), expired()]);
        assert!(command
            .command(false, Duration::from_secs(1), &cancel)
            .is_err());
        assert_eq!(command.transport.requests.len(), 2);
    }

    #[test]
    fn malformed_http_json_false_and_missing_result_are_redacted() {
        for (status, bytes) in [
            (429, b"SECRET".as_slice()),
            (500, b"SECRET"),
            (302, b"SECRET"),
            (200, b"SECRET"),
            (200, br#"{"success":false,"code":"1004","msg":"SECRET"}"#),
            (200, br#"{"success":true}"#),
            (200, br#"{"success":"true","result":true}"#),
        ] {
            let error = parse_response(status, bytes).err().map(api_error).unwrap();
            assert!(!error.to_string().contains("SECRET"));
        }
        for result in [json!(null), json!(false), json!({}), json!("true")] {
            let mut cloud = cloud(vec![token(), success(result)]);
            assert!(cloud
                .command(true, Duration::from_secs(1), &AtomicBool::new(false))
                .is_err());
        }
    }

    #[test]
    fn missing_nonboolean_duplicate_switch_and_offline_fail_closed() {
        for functions in [
            json!([]),
            json!([{"code":"switch_1","type":"Integer"}]),
            json!([{"code":"switch_1","type":"Boolean"},{"code":"switch_1","type":"Boolean"}]),
        ] {
            let mut cloud = cloud(vec![token(), success(json!({"functions": functions}))]);
            assert!(cloud
                .validate(Duration::from_secs(1), &AtomicBool::new(false))
                .is_err());
            assert_eq!(cloud.transport.requests.len(), 2);
        }
        let mut offline = cloud(vec![
            token(),
            success(json!({"functions":[{"code":"switch_1","type":"Boolean"}]})),
            success(json!({"online":false})),
        ]);
        assert!(offline
            .validate(Duration::from_secs(1), &AtomicBool::new(false))
            .unwrap_err()
            .to_string()
            .contains("offline"));
        for statuses in [
            json!([]),
            json!([{"code":"switch_1","value":"true"}]),
            json!([{"code":"switch_1","value":true},{"code":"switch_1","value":false}]),
        ] {
            let mut cloud = cloud(vec![token(), success(statuses)]);
            assert!(cloud
                .status(Duration::from_secs(1), &AtomicBool::new(false))
                .is_err());
        }
    }

    #[test]
    fn credentials_secure_descriptor_checks_and_no_parser_leak() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.toml");
        assert!(Credentials::load(&path).is_err());
        std::fs::write(
            &path,
            "client_id = 'synthetic-id'\nclient_secret = 'synthetic-secret'\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Credentials::load(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(Credentials::load(&path).is_ok());
        symlink(&path, dir.path().join("link")).unwrap();
        assert!(Credentials::load(&dir.path().join("link")).is_err());
        std::fs::hard_link(&path, dir.path().join("hard")).unwrap();
        assert!(Credentials::load(&path).is_err());
        std::fs::remove_file(dir.path().join("hard")).unwrap();
        std::fs::write(&path, "SECRET SOURCE LINE = invalid").unwrap();
        assert!(!Credentials::load(&path)
            .err()
            .unwrap()
            .to_string()
            .contains("SECRET SOURCE LINE"));
    }

    #[test]
    fn cancelled_or_zero_budget_never_sends() {
        let mut cloud = cloud(vec![]);
        assert!(matches!(
            cloud.status(Duration::from_secs(1), &AtomicBool::new(true)),
            Err(Error::Cancelled)
        ));
        assert!(cloud
            .status(Duration::ZERO, &AtomicBool::new(false))
            .is_err());
        assert!(cloud.transport.requests.is_empty());
    }

    #[test]
    fn actual_workflow_uses_signed_transport_not_mocked_cloud_logic() {
        use crate::{
            backend::{workflow, Event, Operation, Phase, Runtime},
            config::{Config, Moonlight, Notifications, Startup},
            process::Output,
        };
        struct Host {
            launched: usize,
        }
        impl Runtime for Host {
            fn now(&self) -> Duration {
                Duration::ZERO
            }
            fn sleep(&mut self, _: Duration, _: &AtomicBool) -> Result<(), Error> {
                panic!("ready immediately")
            }
            fn probe(&mut self, _: Duration, _: &AtomicBool) -> Result<Output, Error> {
                Ok(Output {
                    success: true,
                    stdout: b"Desktop\n".to_vec(),
                    timed_out: false,
                })
            }
            fn stream(&mut self, _: &AtomicBool, emit: &dyn Fn(Event)) -> Result<(), Error> {
                self.launched += 1;
                emit(Event::Phase(Phase::Streaming));
                Ok(())
            }
            fn notify(&mut self, _: &str) {}
        }
        for operation in [Operation::Start, Operation::Check, Operation::PlugOff] {
            let on = operation != Operation::Start;
            let mut replies = vec![
                token(),
                success(json!({"functions":[{"code":"switch_1","type":"Boolean"}]})),
                success(json!({"online":true})),
                status(on),
            ];
            if operation != Operation::Check {
                replies.extend([success(json!(true)), status(!on)]);
            }
            let mut cloud = cloud(replies);
            let config = Config {
                tuya: cloud.config.clone(),
                moonlight: Moonlight {
                    executable: "not-invoked".into(),
                    host: "fake".into(),
                    app: "Desktop".into(),
                    stream_args: vec![],
                },
                startup: Startup {
                    timeout_seconds: 5,
                    poll_interval_seconds: 1,
                    probe_timeout_seconds: 1,
                    http_timeout_seconds: 1,
                },
                notifications: Notifications { enabled: false },
            };
            let mut host = Host { launched: 0 };
            workflow(
                &config,
                operation,
                &AtomicBool::new(false),
                &|_| {},
                || operation == Operation::PlugOff,
                &mut cloud,
                &mut host,
            )
            .unwrap();
            let commands: Vec<_> = cloud
                .transport
                .requests
                .iter()
                .filter(|r| r.method == "POST")
                .collect();
            if operation == Operation::Check {
                assert!(commands.is_empty());
            } else {
                assert_eq!(commands.len(), 1);
                let body: Value = serde_json::from_slice(&commands[0].body).unwrap();
                assert_eq!(body["commands"][0]["value"], operation == Operation::Start);
            }
            assert_eq!(host.launched, usize::from(operation == Operation::Start));
            assert!(cloud.transport.replies.is_empty());
        }
    }
}
