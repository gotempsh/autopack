const port = Number(Deno.env.get("PORT") ?? 3000);

Deno.serve({ port, hostname: "0.0.0.0" }, () =>
  new Response("hello from autopack\n", {
    headers: { "content-type": "text/plain" },
  }));
