const port = Number(process.env.PORT ?? 3000);

Bun.serve({
  port,
  hostname: "0.0.0.0",
  fetch: () =>
    new Response("hello from autopack\n", {
      headers: { "content-type": "text/plain" },
    }),
});

console.log(`listening on ${port}`);
