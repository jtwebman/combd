defmodule Million.Application do
  @moduledoc false
  use Application

  @impl true
  def start(_type, _args) do
    :persistent_term.put(:million_index, File.read!(Application.app_dir(:million, "priv/index.html")))
    port = String.to_integer(System.get_env("PORT", "8080"))

    chunks = for n <- 0..(Million.chunks() - 1), do: Supervisor.child_spec({Million.Chunk, n}, id: {:chunk, n})

    children =
      [Million.Boxes] ++
        chunks ++
        [
          # ThousandIsland's default cap is 100 acceptors x 16,384
          # connections each: far past what fits in a GB, so left alone.
          # Each socket's user-space receive buffer is 9 KB by default (the
          # kernel's buffer size, capped); million's pages send ~40 bytes.
          {Bandit,
           plug: Million.Router,
           ip: :any,
           port: port,
           startup_log: :info,
           thousand_island_options: [transport_options: [buffer: 2048]]}
        ]

    Supervisor.start_link(children, strategy: :one_for_one, name: Million.Supervisor)
  end
end
