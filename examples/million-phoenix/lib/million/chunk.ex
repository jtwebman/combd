defmodule Million.Chunk do
  @moduledoc """
  One per 15,625 boxes (`0` to `63`): the bits, in memory and in its own
  SQLite file, and the socket processes watching it (monitored, so a page
  that leaves is forgotten at once, and expiring 60 s after their last
  renewal, as million's chunks forget viewers).

  Watchers hear a chunk's changes on a 150 ms beat: one message each per
  beat with every change since the last. At the beat the changes are
  written in one transaction (on disk before anyone sees them), the
  message is encoded once, and the same binary goes to every watcher: a
  large binary is shared between processes, not copied.
  """
  use GenServer
  import Bitwise

  @beat_ms 150
  @told_ms 1000
  @sub_ms 60_000
  @prune_ms 10_000

  def start_link(n), do: GenServer.start_link(__MODULE__, n, name: Million.chunk(n))

  @impl true
  def init(n) do
    db = Million.open_db("chunk-#{n}.sqlite")
    :ok = Exqlite.Sqlite3.execute(db, "CREATE TABLE IF NOT EXISTS on_ (i INTEGER PRIMARY KEY)")
    on = Million.rows(db, "SELECT i FROM on_") |> Enum.map(fn [i] -> i end)
    bits = Enum.reduce(on, :binary.copy(<<0>>, div(Million.chunk_size() + 7, 8)), &put(&2, &1, true))
    {:ok, ins} = Exqlite.Sqlite3.prepare(db, "INSERT OR IGNORE INTO on_ (i) VALUES (?1)")
    {:ok, del} = Exqlite.Sqlite3.prepare(db, "DELETE FROM on_ WHERE i = ?1")

    s = %{
      n: n,
      db: db,
      ins: ins,
      del: del,
      bits: bits,
      checked: length(on),
      # Watching: pid => until (ms).
      subs: %{},
      pruned: 0,
      # Every box, as a new watcher gets it: built again only after a change.
      all: nil,
      # This beat's changes, newest first.
      pending: [],
      beat: false,
      # The total hears from a chunk with boxes checked as soon as it starts.
      dirty: on != [],
      told: 0
    }

    {:ok, arm(s)}
  end

  @impl true
  # Watch (or keep watching): every box the first time, then each change.
  def handle_cast({:sub, pid}, s) do
    now = now()
    s = if now - s.pruned >= @prune_ms, do: prune(s, now), else: s

    s =
      if Map.has_key?(s.subs, pid) do
        s
      else
        Process.monitor(pid)
        s = %{s | all: s.all || all(s)}
        send(pid, {:push, s.all})
        s
      end

    {:noreply, %{s | subs: Map.put(s.subs, pid, now + @sub_ms)}}
  end

  def handle_cast({:unsub, pid}, s), do: {:noreply, %{s | subs: Map.delete(s.subs, pid)}}

  # `on` is what the box should be, not a toggle: two people clicking at
  # once both get what they clicked.
  def handle_cast({:set, i, on}, s) do
    if bit(s.bits, i) == on do
      {:noreply, s}
    else
      s = %{
        s
        | bits: put(s.bits, i, on),
          all: nil,
          checked: s.checked + if(on, do: 1, else: -1),
          pending: [{i, on} | s.pending],
          dirty: true
      }

      {:noreply, arm(s)}
    end
  end

  @impl true
  def handle_info(:beat, s) do
    s = %{s | beat: false}
    s = if s.pending == [], do: s, else: flush(s)
    now = now()

    s =
      if s.dirty and now - s.told >= @told_ms do
        GenServer.cast(Million.Boxes, {:count, s.n, s.checked})
        %{s | dirty: false, told: now}
      else
        s
      end

    {:noreply, if(s.dirty, do: arm(s), else: s)}
  end

  def handle_info({:DOWN, _, :process, pid, _}, s), do: {:noreply, %{s | subs: Map.delete(s.subs, pid)}}

  # The beat's changes: on disk, then to every watcher.
  defp flush(s) do
    sets = Enum.reverse(s.pending)
    :ok = Exqlite.Sqlite3.execute(s.db, "BEGIN")
    for {i, on} <- sets, do: Million.run(s.db, if(on, do: s.ins, else: s.del), [i])
    :ok = Exqlite.Sqlite3.execute(s.db, "COMMIT")

    msg =
      case sets do
        [{i, on}] ->
          ~s({"t":"set","n":#{s.n},"i":#{i},"v":#{on}})

        _ ->
          c = Enum.map_intersperse(sets, ",", fn {i, on} -> ["[", Integer.to_string(i), ",", Atom.to_string(on), "]"] end)
          IO.iodata_to_binary([~s({"t":"sets","n":), Integer.to_string(s.n), ~s(,"c":[), c, "]}"])
      end

    for {pid, _} <- s.subs, do: send(pid, {:push, msg})
    %{s | pending: []}
  end

  defp prune(s, now), do: %{s | subs: Map.filter(s.subs, fn {_, until} -> until > now end), pruned: now}

  defp all(s), do: ~s({"t":"all","n":#{s.n},"bits":"#{Base.encode16(s.bits, case: :lower)}"})

  defp arm(%{beat: true} = s), do: s
  defp arm(%{dirty: false} = s), do: s

  defp arm(s) do
    Process.send_after(self(), :beat, @beat_ms)
    %{s | beat: true}
  end

  defp bit(bits, i), do: (:binary.at(bits, i >>> 3) &&& 1 <<< (i &&& 7)) != 0

  defp put(bits, i, on) do
    k = i >>> 3
    <<pre::binary-size(^k), b, post::binary>> = bits
    b = if on, do: b ||| 1 <<< (i &&& 7), else: b &&& bnot(1 <<< (i &&& 7))
    <<pre::binary, b, post::binary>>
  end

  defp now, do: System.system_time(:millisecond)
end
