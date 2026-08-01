// A dependency-free HTTP responder built on POSIX sockets, so the example
// exercises the build rather than SwiftNIO's dependency graph.
#if canImport(Glibc)
import Glibc
#endif
import Foundation

let port = UInt16(ProcessInfo.processInfo.environment["PORT"] ?? "3000") ?? 3000

let listener = socket(AF_INET, Int32(SOCK_STREAM.rawValue), 0)
guard listener >= 0 else { fatalError("socket() failed") }

var yes: Int32 = 1
setsockopt(listener, SOL_SOCKET, SO_REUSEADDR, &yes, socklen_t(MemoryLayout<Int32>.size))

var address = sockaddr_in()
address.sin_family = sa_family_t(AF_INET)
address.sin_port = port.bigEndian
address.sin_addr = in_addr(s_addr: INADDR_ANY)

let bound = withUnsafePointer(to: &address) { pointer in
    pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { sockaddrPointer in
        bind(listener, sockaddrPointer, socklen_t(MemoryLayout<sockaddr_in>.size))
    }
}
guard bound >= 0 else { fatalError("bind() failed") }
guard listen(listener, 16) >= 0 else { fatalError("listen() failed") }

// `print` is line-buffered to a pipe under Docker; setvbuf keeps the
// startup line visible without touching the global `stdout` var, which
// Swift 6 rejects as not concurrency-safe.
print("listening on \(port)")

let body = "hello from autopack\n"
let response = """
HTTP/1.1 200 OK\r
Content-Type: text/plain\r
Content-Length: \(body.utf8.count)\r
Connection: close\r
\r
\(body)
"""

while true {
    let client = accept(listener, nil, nil)
    if client < 0 { continue }
    var scratch = [UInt8](repeating: 0, count: 1024)
    _ = recv(client, &scratch, scratch.count, 0)
    _ = response.withCString { send(client, $0, strlen($0), 0) }
    close(client)
}
