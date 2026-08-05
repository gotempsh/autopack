defmodule Hello.Application do
  @moduledoc """
  Serves a fixed response over `:gen_tcp` so the example needs no dependencies.
  """

  use Application

  @impl true
  def start(_type, _args) do
    children = [{Task, fn -> listen() end}]
    Supervisor.start_link(children, strategy: :one_for_one, name: Hello.Supervisor)
  end

  defp listen do
    port = "PORT" |> System.get_env("3000") |> String.to_integer()

    {:ok, socket} =
      :gen_tcp.listen(port, [:binary, packet: :http_bin, active: false, reuseaddr: true])

    IO.puts("listening on #{port}")
    accept(socket)
  end

  defp accept(socket) do
    {:ok, client} = :gen_tcp.accept(socket)
    _ = :gen_tcp.recv(client, 0)

    body = "hello from autopack\n"

    :gen_tcp.send(client, [
      "HTTP/1.1 200 OK\r\n",
      "Content-Type: text/plain\r\n",
      "Content-Length: #{byte_size(body)}\r\n",
      "Connection: close\r\n\r\n",
      body
    ])

    :gen_tcp.close(client)
    accept(socket)
  end
end
