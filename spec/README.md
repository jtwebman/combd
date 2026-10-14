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

`Replication.tla` is D3, fast writes: peers confirm each write, and the
bucket gets batches later. A write is acknowledged once every peer has it. A
new owner fences one peer of the epoch it took over from, then reads the
peer's tail along with the bucket. Peers drop entries only after the bucket
has them. A clean handoff seals the epoch: upload everything, then step down.

Same three invariants. The model has two owners on different nodes, two
peers, crashes, and a superseded owner that keeps sending and uploading until
it notices. 35.7 million states, no errors. TLC found one real bug in the
first design: a new owner skipped the fence because the old epoch had no
manifest yet, and the old owner then wrote one late and served writes that
were lost. The fix: fence unless the old epoch was sealed.

Six known-bad versions must fail:

| Bug | Caught by |
|---|---|
| acknowledge when one peer has it | NoLostAckedWrite |
| never fence | NoLostAckedWrite |
| read the bucket before fencing | NoLostAckedWrite |
| peers drop entries before upload | NoLostAckedWrite |
| seal before everything is uploaded | NoLostAckedWrite |
| fence only if the old epoch wrote a manifest | NoLostAckedWrite |

Run `./check.sh` (needs Java; it fetches TLC 1.7.4 and checks its hash). The whole run takes about 2.5 minutes.

Not modeled yet: reads (they pass the same gate as writes), leases and
clocks (liveness only), many objects, and checkpoints of old epochs.
