defmodule Demo.Application do
  @moduledoc "Boots the HTTP server. Unlike the bare-release example, this one pulls real hex dependencies."

  use Application

  @impl true
  def start(_type, _args) do
    port = "PORT" |> System.get_env("3000") |> String.to_integer()

    children = [
      {Bandit, plug: Demo.Router, scheme: :http, ip: {0, 0, 0, 0}, port: port}
    ]

    Supervisor.start_link(children, strategy: :one_for_one, name: Demo.Supervisor)
  end
end
