# Specs

`Catalog.tla` is the catalog: deploys, vars, secrets, the state of every
app. The bucket keeps it as an append-only log, and every node keeps a copy
(SQLite, in memory or on disk). A write reads the log's head, checks its
change against the state there (here: no two apps hold one domain), writes
the next slot only if it's empty (a conditional write), and is acknowledged
only after that. Two writers can never both take a slot, so there's one
order and no leader is needed for safety. Writers push each change to
every node; pushes may be lost, repeated, or come in any order, and a node
applies entry n+1 only after n, fetching gaps from the bucket. A node may
crash and keep its copy (disk) or lose it (memory).

The log is trimmed: a node writes a snapshot (the state at slot k, from its
copy), and only then are slots up to k deleted. A deleted slot is empty
again, so a slow writer whose check used an old head could write it, and
"only if empty" would let it. So after a write, the writer reads the
snapshot: if it covers the slot, the write didn't count, and it checks
again. But the stray bytes stay in the slot, and a node catching up could
read them: so a reader, too, reads the snapshot again after reading slots,
and if it covers them, starts from the snapshot instead. The first version
of this spec let only writers meet a trimmed slot (readers read the true
log, not the bucket's bytes); the simulator found the reader's half, and
the spec has it now.

Four invariants: no acknowledged change is lost, every node's copy is the
start of the log, no two apps ever hold one domain (in the log or any
copy), and every node can still catch up. 58.2 million states, no errors
(13 minutes). Seven known-bad versions must fail:

| Bug | Caught by |
|---|---|
| acknowledge before the write | NoLostAckedChange |
| write a slot without "only if empty" | NoLostAckedChange |
| apply pushes in any order | CopyIsPrefix |
| check against this node's copy, but write at the head | OneAppPerDomain |
| acknowledge a write without checking the snapshot | NoLostAckedChange |
| trim slots the snapshot doesn't cover yet | CanCatchUp |
| catch up without reading the snapshot again | CopyIsPrefix |

The last is the first deploy design: each app's own pointer, checked
against the deploying node's view. Two deploys on two nodes could both
claim a domain. TLC shows it; the log fixes it.

Leases, heartbeats, and leaders aren't in it: they're for liveness. The
ownership spec below already lets any process claim at any time, because
its safety never rested on leases, so moving node heartbeats from the
bucket to the wire changes no proof.

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

**Read leases.** A peer that takes an entry (or an empty round) promises
its epoch: a fence for a newer epoch must wait the promise out. Once every
peer has promised its epoch, the owner holds a lease and answers reads with
no round. Time enters only as one assumption, `Expire`: a promise lapses
only after the owner's lease has ended (the owner trusts less than was
promised, counted from before it was made). New invariant, `NoStaleRead`: a
read under a lease sees every acknowledged write. 95.9 million states, no
errors; with a wiped peer too (`RepWipe.cfg` now has leases), 624 million
states, no errors. Two more known-bad versions must fail:

| Bug | Caught by |
|---|---|
| a fence ignores a peer's promise | NoStaleRead |
| a promise lapses while the owner still trusts it | NoStaleRead |

A peer that has told a newer owner's fence to wait renews the older epoch's
promise no more (in the code, it answers `Kept`), so an old owner that keeps
going can't hold a takeover off forever. That needs no change here: `Renew`
may always not happen, so fewer renewals are behaviors this spec already
checks, and `GetLease` already asks for every peer's promise. (The model
checks safety only; the simulator checks that a takeover then happens.)

The peers are interchangeable, and so are the writes, so TLC checks one of
each set of mirror-image states (`SYMMETRY`); that's sound for invariants,
which is all we check.

**Peer sets, and a node replaced** (2026-10-28). A drill on real nodes
found an object that no node would serve: one node replaced with an empty
disk, then its owner lost. The owner had picked itself as one of its own
two peers, so the object had two copies where the fleet counted three, and
the model couldn't show it: it kept owners and peers apart, with one peer
set for every epoch. Now each epoch has its own set (`pset`), two of a pool
of three peer machines, never its owner's. A claim after no epoch or a
sealed one picks any set. A claim after an unsealed one keeps that epoch's,
so its fence covers it. A peer that lost its disk relearns lazily, on first
touch (`Learn`), as the code does, and speaks for every epoch whose
manifest was written after it came back. The code reads that from the
bucket's times, and may know less, so `Learn` may pick any `born` up to the
record's epoch. Loading from a sealed epoch needs only the fence: the
bucket has all of it, and the peers fenced may be another set that holds
none of it. One more known-bad version must fail:

| Bug | Caught by |
|---|---|
| load with no peer's tail, sealed or not | NoLostAckedWrite |

The model checks safety. Liveness is the simulator's
(`a_lost_disk_and_a_lost_node_strand_nothing`: a replaced node, then a node
lost for good, in 3,000 runs), and the drill's
(`combd/bench/drill/replace-node.sh`).

With peer sets, the model with no wipes checks clean in 213,050,094
distinct states (an hour on 14 cores). The wipe model no longer finishes on
a laptop: TLC stopped at 1.1 billion distinct states, every behavior up to
28 steps, with 389 million left and the disk full. No error in any of it.
So `check.sh` walks it at random instead, for 30 minutes (`WIPE_SECS`).
Random runs find both known wipe bugs, in about 8 minutes and 2. An hour
of them, 3.67 billion states, found nothing on the fix (2026-10-29).
That's evidence, not proof. The whole space needs a bigger machine.

Run `./check.sh` (needs Java; it fetches TLC 1.7.4 and checks its hash). The
whole run takes about two hours: an hour for the model with no wipes, half
an hour for the wipe model.

Not modeled yet: reads (they pass the same gate as writes), leases and
clocks (liveness only), many objects, and checkpoints of old epochs.
