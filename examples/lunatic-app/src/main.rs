//! A Lunatic actor serving a fixed HTTP response, compiled to wasm32-wasip1
//! and executed by the Lunatic runtime.
use lunatic::net::TcpListener;
use std::io::{Read, Write};

fn main() {
    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    let listener = TcpListener::bind(format!("0.0.0.0:{port}")).expect("bind failed");
    println!("listening on {port}");

    let body = "hello from autopack\n";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );

    while let Ok((mut stream, _)) = listener.accept() {
        let mut scratch = [0u8; 1024];
        let _ = stream.read(&mut scratch);
        let _ = stream.write_all(response.as_bytes());
    }
}
