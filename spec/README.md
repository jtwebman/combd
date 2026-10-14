# Specs

`Ownership.tla` is the D2 protocol for who owns a Durable Object, with the
bucket as the only coordinator: a compare-and-swap ownership record, a new
epoch for every activation, a manifest that says which data the new owner
took over, and an output gate that re-reads the record before any write is
acknowledged.

TLC checks three invariants: one writer per epoch, no lost acknowledged write,
and no history rewrite (what an owner proved is where every later owner
starts). The model has crashes, lost replies, uploads that land late, and two
processes with the same node name (a frozen process and its restart). Clocks
aren't modeled: safety must hold with any clock.

Four known-bad versions must fail, each on a different invariant:

| Bug | Caught by |
|---|---|
| acknowledge without re-reading the record | NoLostAckedWrite |
| read the old history before winning the CAS | NoLostAckedWrite |
| no manifest: rebuild from every segment | NoHistoryRewrite |
| a same-node wake keeps its epoch | OneWriterPerEpoch |

Run `./check.sh` (needs Java; it fetches TLC 1.7.4 and checks its hash).

Not modeled yet: reads (they pass the same gate as writes), leases and
clocks (liveness only), many objects, and checkpoints of old epochs.
