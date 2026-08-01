// Uses only the JDK's HTTP server, so the example exercises the assembly build
// rather than a framework's dependency graph.
import com.sun.net.httpserver.{HttpExchange, HttpHandler, HttpServer}
import java.net.InetSocketAddress
import java.nio.charset.StandardCharsets

@main def run(): Unit =
  val port = sys.env.getOrElse("PORT", "3000").toInt
  val server = HttpServer.create(InetSocketAddress("0.0.0.0", port), 0)

  server.createContext("/", new HttpHandler:
    def handle(exchange: HttpExchange): Unit =
      val body = "hello from autopack\n".getBytes(StandardCharsets.UTF_8)
      exchange.getResponseHeaders.add("Content-Type", "text/plain")
      exchange.sendResponseHeaders(200, body.length)
      val out = exchange.getResponseBody
      try out.write(body) finally out.close()
  )

  server.start()
  println(s"listening on $port")
