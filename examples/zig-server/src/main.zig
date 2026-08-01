//! A tiny HTTP responder on raw sockets: the point is the build, not the I/O.
const std = @import("std");

pub fn main() !void {
    var gpa = std.heap.GeneralPurposeAllocator(.{}){};
    defer _ = gpa.deinit();
    const allocator = gpa.allocator();

    const port_text = std.process.getEnvVarOwned(allocator, "PORT") catch
        try allocator.dupe(u8, "3000");
    defer allocator.free(port_text);
    const port = try std.fmt.parseInt(u16, port_text, 10);

    const address = try std.net.Address.parseIp("0.0.0.0", port);
    var server = try address.listen(.{ .reuse_address = true });
    defer server.deinit();

    std.debug.print("listening on {d}\n", .{port});

    const body = "hello from autopack\n";
    const response = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 20\r\nConnection: close\r\n\r\n" ++ body;

    while (true) {
        const connection = server.accept() catch continue;
        defer connection.stream.close();

        var scratch: [1024]u8 = undefined;
        _ = connection.stream.read(&scratch) catch {};
        _ = connection.stream.writeAll(response) catch {};
    }
}
