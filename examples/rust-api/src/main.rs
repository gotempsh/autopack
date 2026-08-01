//! A dependency-free HTTP server, so the example exercises the build rather
//! than the crates.io graph.

use std::env;
use std::io::{Read, Write};
use std::net::TcpListener;

fn main() -> std::io::Result<()> {
    let port = env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    let listener = TcpListener::bind(format!("0.0.0.0:{port}"))?;
    println!("listening on {port}");

    let body = "hello from autopack\n";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(_) => continue,
        };
        let mut scratch = [0u8; 1024];
        let _ = stream.read(&mut scratch);
        let _ = stream.write_all(response.as_bytes());
    }

    Ok(())
}
