use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use rocket::response::stream::ByteStream;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

#[rocket::post("/v1/systemone", data = "<body>")]
fn limited(
    body: String,
    calls: &rocket::State<Arc<AtomicUsize>>,
) -> String {
    calls.fetch_add(1, Ordering::SeqCst);
    body
}
#[rocket::post("/chat")]
fn stream() -> ByteStream![Vec<u8>] {
    ByteStream! {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        yield b"data: done\n\n".to_vec();
    }
}

#[tokio::test]
async fn transport_deadline_covers_preprocessing_and_headers_without_limiting_other_streams() {
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = socket.local_addr().unwrap().port();
    drop(socket);
    let calls = Arc::new(AtomicUsize::new(0));
    let config = rocket::Config {
        port,
        address: "127.0.0.1".parse().unwrap(),
        request_timeout: Some(1),
        request_timeout_paths: vec!["/v1/systemone".into()],
        log_level: rocket::config::LogLevel::Off,
        ..rocket::Config::debug_default()
    };
    let rocket = rocket::custom(config)
        .manage(calls.clone())
        .register(
            "/v1",
            rocket::catchers![super::systemone::deadline_exceeded, super::systemone::unsupported_http_version],
        )
        .mount("/", rocket::routes![limited, stream])
        .ignite()
        .await
        .unwrap();
    let shutdown = rocket.shutdown();
    let server = tokio::spawn(rocket.launch());
    let ready = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(connection) = TcpStream::connect(("127.0.0.1", port)).await {
                break connection;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(ready);

    for path in ["/v1/systemone", "/v1//systemone", "/v1/%73ystemone"] {
        let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        socket.write_all(format!("POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: text/plain\r\nContent-Length: 1000\r\nX-Request-Deadline: 99999999999\r\n\r\nx").as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_millis(1800), socket.read_to_end(&mut response)).await.unwrap().unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 504"), "{response}");
        assert!(response.contains("deadline_exceeded"));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    socket.write_all(b"POST /v1/systemone HTTP/1.1\r\nHost:").await.unwrap();
    tokio::time::sleep(Duration::from_millis(650)).await;
    socket.write_all(b" localhost\r\nContent-Length: 1000\r\n\r\nx").await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_millis(650), socket.read_to_end(&mut response)).await.unwrap().unwrap();
    assert!(String::from_utf8(response).unwrap().starts_with("HTTP/1.1 504"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    socket
        .write_all(b"POST /chat HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut response)).await.unwrap().unwrap();
    let response = String::from_utf8(response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(response.contains("data: done"));
    shutdown.notify();
    server.await.unwrap().unwrap();
}
