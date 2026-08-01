package demo;

import com.sun.net.httpserver.HttpServer;
import java.io.IOException;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;

/** Uses only the JDK so the example needs no third-party dependencies. */
public final class Main {
  public static void main(String[] args) throws IOException {
    String portValue = System.getenv("PORT");
    int port = portValue == null ? 3000 : Integer.parseInt(portValue);

    HttpServer server = HttpServer.create(new InetSocketAddress("0.0.0.0", port), 0);
    server.createContext("/", exchange -> {
      byte[] body = "hello from autopack\n".getBytes(StandardCharsets.UTF_8);
      exchange.getResponseHeaders().add("Content-Type", "text/plain");
      exchange.sendResponseHeaders(200, body.length);
      try (OutputStream out = exchange.getResponseBody()) {
        out.write(body);
      }
    });
    server.start();
    System.out.println("listening on " + port);
  }
}
