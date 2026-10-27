defmodule X.Application do
  @moduledoc false
  use Application

  @impl true
  def start(_type, _args) do
    # DATABASE_URL: postgres://user:pass@host:port/db
    url = URI.parse(System.fetch_env!("DATABASE_URL"))
    [user, pass] = String.split(url.userinfo, ":", parts: 2)

    children = [
      # 8 connections, as combd's pool (`max = 8`).
      {Postgrex,
       name: X.DB,
       hostname: url.host,
       port: url.port || 5432,
       username: user,
       password: pass,
       database: String.trim_leading(url.path, "/"),
       pool_size: String.to_integer(System.get_env("POOL_SIZE", "8"))},
      {Bandit, plug: X.Router, ip: :any, port: String.to_integer(System.get_env("PORT", "8080")), startup_log: :info}
    ]

    Supervisor.start_link(children, strategy: :one_for_one, name: X.Supervisor)
  end
end
