//! The end-to-end claim, checked against a real socket: a span recorded through
//! `tracing` leaves this process as an OTLP request, carrying the service name
//! and the headers the configuration asked for.
//!
//! Its own test binary because installing a global subscriber can only happen
//! once per process, and what is being checked here is precisely that install.
//!
//! The collector is hand-rolled over `TcpListener` rather than pulled in as a
//! dependency: the assertion is about bytes on a socket, and a test that needs a
//! web framework to make it has moved a step away from what it verifies.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Sender, channel};
use std::thread;
use std::time::Duration;

/// One OTLP request as it arrived.
struct Received {
    headers: Vec<(String, String)>,
    path: String,
    body: Vec<u8>,
}

impl Received {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Accepts requests until the test drops it, reporting each on the channel.
fn collector(sink: Sender<Received>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let port = listener.local_addr().expect("an address").port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            if let Some(received) = serve(stream) {
                let _ = sink.send(received);
            }
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn serve(mut stream: TcpStream) -> Option<Received> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);

    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let path = request_line.split_whitespace().nth(1)?.to_owned();

    let mut headers = Vec::new();
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        let (name, value) = (name.trim().to_owned(), value.trim().to_owned());
        if name.eq_ignore_ascii_case("content-length") {
            length = value.parse().ok()?;
        }
        headers.push((name, value));
    }

    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;

    // An empty ExportTraceServiceResponse, which is what a collector answers on
    // full success.
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/x-protobuf\r\nContent-Length: 0\r\n\r\n",
        )
        .ok()?;
    stream.flush().ok()?;

    Some(Received {
        headers,
        path,
        body,
    })
}

#[test]
fn a_span_leaves_the_process_with_its_service_name_and_headers() {
    let (sink, received) = channel();
    let endpoint = collector(sink);

    let telemetry = cartografo_telemetry::Telemetry::builder("test-service")
        .without_env()
        .endpoint(&endpoint)
        .environment("test")
        .version("9.9.9")
        .cloudflare_access("an-id.access", "a-secret")
        .header("x-extra", "kept")
        .log_format(cartografo_telemetry::LogFormat::Off)
        .try_install()
        .expect("installs");

    assert!(telemetry.is_exporting());
    assert_eq!(
        telemetry.endpoint(),
        Some(format!("{endpoint}/v1/traces").as_str()),
        "the signal path is appended to the base URL"
    );

    {
        let span = tracing::info_span!("a-unit-of-work");
        let _entered = span.enter();
    }
    telemetry.force_flush().expect("flushes");

    let request = received
        .recv_timeout(Duration::from_secs(10))
        .expect("the collector received an export request");

    assert_eq!(request.path, "/v1/traces");
    assert_eq!(
        request.header("content-type"),
        Some("application/x-protobuf"),
        "http-proto, not http-json: a collector that only speaks protobuf is the common case"
    );
    assert_eq!(request.header("CF-Access-Client-Id"), Some("an-id.access"));
    assert_eq!(request.header("CF-Access-Client-Secret"), Some("a-secret"));
    assert_eq!(request.header("x-extra"), Some("kept"));

    // Protobuf keeps strings as their raw bytes, so the resource and the span are
    // findable without decoding the message — which would mean depending on the
    // generated types to assert that the generated types were used.
    let body = request.body;
    assert!(
        find(&body, b"test-service"),
        "the service name has to reach the collector or every span is unattributed"
    );
    assert!(find(&body, b"a-unit-of-work"), "the span itself is missing");
    assert!(find(&body, b"9.9.9"), "service.version is missing");
    assert!(
        find(&body, b"deployment.environment.name"),
        "the environment attribute is missing, and prod and dev would share a view"
    );

    telemetry.try_shutdown().expect("stops cleanly");
}

fn find(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
