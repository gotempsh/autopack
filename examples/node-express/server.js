const http = require("node:http");

const port = Number(process.env.PORT ?? 3000);

http
  .createServer((_req, res) => {
    res.writeHead(200, { "content-type": "text/plain" });
    res.end("hello from autopack\n");
  })
  .listen(port, "0.0.0.0", () => {
    console.log(`listening on ${port}`);
  });
