defmodule Million do
  @moduledoc """
  One Million Checkboxes on the BEAM: examples/million (combd) ported, with
  the same page, protocol, and limits, so the same load client drives both.

  - `Million.Chunk`: a process per 15,625 boxes (64 of them): the bits, in
    memory and in its SQLite, and the sockets watching. Each beat's changes
    are written in one transaction, then encoded once and sent, as the same
    binary, to every watcher's process.
  - `Million.Boxes`: one process, each chunk's count, for the total.
  - `Million.Socket`: a process per page socket (Bandit's, running this
    WebSock handler): what the page watches, and its sets.
  - `Million.Router`: the page, the count, and the upgrade.
  """

  @chunk 15_625
  @chunks 64

  def chunk_size, do: @chunk
  def chunks, do: @chunks

  @doc "A chunk's process."
  def chunk(n), do: :"chunk_#{n}"

  def data_dir, do: System.get_env("MILLION_DATA", "/tmp/data")

  @doc "SQLite, as million's host has it: WAL, and a commit is on disk before anything it wrote is shown."
  def open_db(name) do
    File.mkdir_p!(data_dir())
    {:ok, db} = Exqlite.Sqlite3.open(Path.join(data_dir(), name))
    :ok = Exqlite.Sqlite3.execute(db, "PRAGMA journal_mode = WAL")
    :ok = Exqlite.Sqlite3.execute(db, "PRAGMA synchronous = FULL")
    db
  end

  def rows(db, sql) do
    {:ok, st} = Exqlite.Sqlite3.prepare(db, sql)
    {:ok, rows} = Exqlite.Sqlite3.fetch_all(db, st)
    :ok = Exqlite.Sqlite3.release(db, st)
    rows
  end

  def run(db, st, args) do
    :ok = Exqlite.Sqlite3.bind(st, args)
    :done = Exqlite.Sqlite3.step(db, st)
    :ok = Exqlite.Sqlite3.reset(st)
  end
end
