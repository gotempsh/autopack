// swift-tools-version:6.0
import PackageDescription

let package = Package(
    name: "swift-server-example",
    targets: [
        .executableTarget(name: "Server", path: "Sources/Server")
    ]
)
