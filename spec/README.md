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
new owner fences one peer, which hands back its tail of every epoch. Then the
owner lists the bucket and picks the last epoch with a manifest. Peers drop
entries only after the bucket has them. If a peer is slow or down, a write
the bucket already has can be acknowledged the D2 way, by re-reading the
record. A clean handoff seals the epoch: upload everything, then step down.
A peer drops older epochs' logs once a newer epoch writes to it, so an owner
that finds a newer manifest when it lists the bucket is stale and stops.

Same three invariants. The model has two owners on different nodes, two
peers, crashes, and a superseded owner that keeps sending and uploading until
it notices. 39.9 million states, no errors. TLC found one real bug in the
first design: a new owner skipped the fence because the old epoch had no
manifest yet, and the old owner then wrote one late and served writes that
were lost. The fix: fence unless the old epoch was sealed. The simulator found
another (the stale owner that has to stop), and it's here now too.

Eight known-bad versions must fail:

| Bug | Caught by |
|---|---|
| acknowledge when one peer has it | NoLostAckedWrite |
| never fence | NoLostAckedWrite |
| read the bucket before fencing | NoLostAckedWrite |
| peers drop entries before upload | NoLostAckedWrite |
| seal before everything is uploaded | NoLostAckedWrite |
| fence only if the old epoch wrote a manifest | NoLostAckedWrite |
| acknowledge what's uploaded without re-reading the record | NoLostAckedWrite |
| a stale owner writes its manifest after a newer one | NoLostAckedWrite |

**A peer that loses its disk** (`RepWipe.cfg`, at most one wipe: two lost
disks at once really can lose writes). It comes back empty, having forgotten
what it held and whom it fenced. So before it answers anything, it reads the
ownership record (epoch R): it refuses every epoch below R, the fence it lost,
and it speaks only for epochs after R. A fence's answer says which, and a new
owner uses a tail only from a peer that can speak for the epoch it needs.
TLC found a bug in the first try: "the peer has a log of that epoch since
the wipe" isn't enough, because the owner may still be re-sending it, so the
copy can be partial. 143.9 million states (with symmetry), no errors. Two
more known-bad versions must fail:

| Bug | Caught by |
|---|---|
| trust a wiped peer's tail for an epoch it may have lost | NoLostAckedWrite |
| a wiped peer doesn't relearn its fence | NoLostAckedWrite |

The peers are interchangeable, and so are the writes, so TLC checks one of
each set of mirror-image states (`SYMMETRY`); that's sound for invariants,
which is all we check.

Run `./check.sh` (needs Java; it fetches TLC 1.7.4 and checks its hash). The
whole run takes about 25 minutes, most of it the wipe model.

Not modeled yet: reads (they pass the same gate as writes), leases and
clocks (liveness only), many objects, and checkpoints of old epochs.
