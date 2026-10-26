defmodule Million.Socket do
  @moduledoc """
  A page's socket, as million's viewer: Bandit runs a process per
  connection, and this is its WebSock handler. All it keeps is the chunks
  it watches and its rate; the chunks send their changes straight to it,
  already encoded, and it writes them out.
  """
  @behaviour WebSock

  @max_watch 4
  @max_sets_per_s 20

  @impl true
  def init(_), do: {:ok, %{lo: 1, hi: 0, sec: 0, sets: 0}}

  @impl true
  def handle_in({text, opcode: :text}, s) do
    case JSON.decode(text) do
      # Then a collection: Bandit can't hibernate a socket's process (as
      # Phoenix's channels do after 15 s idle), and a page is idle until
      # its next renewal, 20 s on. This shrinks its heap to what it keeps.
      {:ok, %{"t" => "range"} = v} ->
        s = range(v, s)
        :erlang.garbage_collect()
        {:ok, s}

      {:ok, %{"t" => "set", "i" => i, "v" => on}} -> set(i, on, s)
      _ -> {:ok, s}
    end
  end

  def handle_in(_, s), do: {:ok, s}

  @impl true
  def handle_info({:push, msg}, s), do: {:push, {:text, msg}, s}
  def handle_info(_, s), do: {:ok, s}

  # Closed: the chunks monitor this process, and forget it as it ends.
  @impl true
  def terminate(_, _), do: :ok

  # What the page shows, boxes [from, to). Sent again every 20 s: that
  # renews the subscriptions.
  defp range(v, s) do
    chunk = Million.chunk_size()
    last = Million.chunks() - 1
    from = whole(v["from"])
    to = whole(v["to"])
    lo = min(div(from, chunk), last)
    hi = Enum.min([div(max(to, from + 1) - 1, chunk), last, lo + @max_watch - 1])
    for n <- s.lo..s.hi//1, n < lo or n > hi, do: GenServer.cast(Million.chunk(n), {:unsub, self()})
    for n <- lo..hi//1, do: GenServer.cast(Million.chunk(n), {:sub, self()})
    %{s | lo: lo, hi: hi}
  end

  defp set(i, on, s) when is_integer(i) and i >= 0 and is_boolean(on) do
    chunk = Million.chunk_size()

    if i >= chunk * Million.chunks() do
      {:ok, s}
    else
      now = System.system_time(:second)
      s = if s.sec == now, do: %{s | sets: s.sets + 1}, else: %{s | sec: now, sets: 1}

      if s.sets > @max_sets_per_s do
        {:push, {:text, ~s({"t":"slow","i":#{i}})}, s}
      else
        GenServer.cast(Million.chunk(div(i, chunk)), {:set, rem(i, chunk), on})
        {:ok, s}
      end
    end
  end

  defp set(_, _, s), do: {:ok, s}

  defp whole(x) when is_integer(x), do: max(x, 0)
  defp whole(x) when is_float(x), do: max(trunc(x), 0)
  defp whole(_), do: 0
end
