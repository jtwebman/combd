defmodule Million.Boxes do
  @moduledoc "One: each chunk's count (each tells it at most once a second), for the total."
  use GenServer

  def start_link(_), do: GenServer.start_link(__MODULE__, nil, name: __MODULE__)

  def total, do: GenServer.call(__MODULE__, :total)

  @impl true
  def init(_) do
    db = Million.open_db("boxes.sqlite")
    :ok = Exqlite.Sqlite3.execute(db, "CREATE TABLE IF NOT EXISTS counts (n INTEGER PRIMARY KEY, checked INTEGER)")
    {:ok, st} = Exqlite.Sqlite3.prepare(db, "INSERT INTO counts (n, checked) VALUES (?1, ?2) ON CONFLICT (n) DO UPDATE SET checked = excluded.checked")
    counts = Million.rows(db, "SELECT n, checked FROM counts") |> Map.new(fn [n, c] -> {n, c} end)
    {:ok, %{db: db, st: st, counts: counts}}
  end

  @impl true
  def handle_cast({:count, n, checked}, s) do
    Million.run(s.db, s.st, [n, checked])
    {:noreply, %{s | counts: Map.put(s.counts, n, checked)}}
  end

  @impl true
  def handle_call(:total, _, s), do: {:reply, s.counts |> Map.values() |> Enum.sum(), s}
end
