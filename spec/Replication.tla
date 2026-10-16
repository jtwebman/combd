--------------------------- MODULE Replication ---------------------------
(***************************************************************************)
(* combd D3: fast writes. Peers confirm each write; the bucket gets batches *)
(* later. Ownership is still a CAS record in the bucket (Ownership.tla),    *)
(* and the record names each epoch's peer set.                              *)
(*                                                                         *)
(* The protocol:                                                           *)
(*   1. Claim: CAS rec to [me, epoch + 1].                                 *)
(*   2. Fence, then load. Unless the epoch we took over from was sealed  *)
(*      (or there was none), fence one peer: it refuses every epoch below  *)
(*      ours from then on, and returns its tail of every epoch in the same *)
(*      call. Only then list the bucket: q is the last epoch with a        *)
(*      manifest. History = manifest[q] + q's uploaded entries + what q's  *)
(*      tail adds. Write it as our manifest.                               *)
(*                                                                         *)
(*      Fence the epoch we took over from even if it never wrote a         *)
(*      manifest: its owner may write one late and start serving, and with *)
(*      no fence nothing would stop it. (TLC found that one.) An object's  *)
(*      peer set stays the same across epochs, so one fence covers every   *)
(*      older epoch. After a sealed epoch, skipping is safe: whoever       *)
(*      started that epoch fenced everything older.                        *)
(*   3. Serve. Send each entry, in order, to every peer; a peer takes it    *)
(*      only if not fenced. Acknowledge once EVERY peer has it. If a peer  *)
(*      is slow or down, an entry the bucket has can be acknowledged after *)
(*      re-reading rec, as in D2.                                          *)
(*   4. In the background, upload entries to the bucket, then tell peers   *)
(*      they may drop what the bucket has.                                 *)
(*   5. Seal (a peer died, or a clean handoff): upload everything, mark    *)
(*      the epoch sealed, and step down. A sealed epoch needs no peers.    *)
(*                                                                         *)
(* Why one fenced peer is enough: an acknowledgement needs every peer, so *)
(* once one refuses, the old owner can't acknowledge more; and that peer   *)
(* holds every entry acknowledged before.                                 *)
(*                                                                         *)
(* Processes crash and lose everything local; their entries live on in the *)
(* peers and the bucket. A superseded owner keeps acting until it notices, *)
(* so late appends and late uploads are in the model. No clocks.           *)
(*                                                                         *)
(* Read leases. A peer that takes an entry (or an empty round) of epoch E  *)
(* promises E for a while: a fence for a newer epoch must wait it out.     *)
(* Once every peer has promised its epoch, the owner holds a lease and may *)
(* answer reads on its own, with no round. Time enters only as one         *)
(* assumption, `Expire`: a peer's promise lapses only after the owner's     *)
(* lease has ended (the owner trusts less than was promised, counted from  *)
(* before it was made, with room for clocks that run apart). The claim:    *)
(* such a read never misses an acknowledged write.                         *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS Procs, NodeOf, Peers, Writes, MaxEpoch,
          MaxWipes,              \* how many times a peer's disk may be lost
          BUG_ACK_ONE,           \* acknowledge when one peer has it, not all
          BUG_NO_FENCE,          \* read a peer's tail without fencing it
          BUG_LOAD_BEFORE_FENCE, \* list the bucket before fencing
          BUG_TRUNC_EARLY,       \* peers drop entries before they're uploaded
          BUG_SEAL_EARLY,        \* seal before everything is uploaded
          BUG_FENCE_IF_MANIFEST, \* fence only if an epoch has a manifest (TLC's find)
          BUG_ACK_NO_PROOF,      \* acknowledge what's uploaded without re-reading rec
          BUG_LATE_MANIFEST,     \* a stale owner writes its manifest after a newer one
          BUG_TRUST_WIPED,       \* trust a wiped peer's tail for an epoch it lost
          BUG_WIPE_FORGETS_FENCE, \* a wiped peer doesn't relearn its fence
          BUG_LEASE_NO_PROMISE,  \* a fence ignores a peer's promise
          BUG_LEASE_OVERTRUST    \* a promise lapses while the owner still trusts it

Epochs == 1..MaxEpoch
NoRec == [node |-> "none", epoch |-> 0]
Min(S) == CHOOSE x \in S : \A y \in S : x <= y
Max(S) == CHOOSE x \in S : \A y \in S : x >= y
IsPrefix(a, b) == Len(a) <= Len(b) /\ SubSeq(b, 1, Len(a)) = a
Range(s) == {s[i] : i \in DOMAIN s}
Take(s, n) == SubSeq(s, 1, n)

VARIABLES
    rec,                    \* the bucket: ownership record
    made, manifest, sealed, \* the bucket: per epoch
    up,                     \* the bucket: entries uploaded per epoch (a prefix of its log)
    plog, ptrunc, fenced,   \* each peer: entries per epoch, how many it dropped, fence
    born,                   \* each peer: 0, or after losing its disk, the epoch it
                            \* relearned; it knows only the epochs after that
    wipes,                  \* disks lost so far
    st, seen, ep, base, log, tail, wm, upseen, \* each process
    issued, ackedIn, proven, \* history, for the invariants
    promised,               \* each peer: the epoch it promised not to be fenced past (0: none)
    lease,                  \* each process: it holds a read lease
    staleRead               \* a lease read missed an acknowledged write

lv == <<promised, lease, staleRead>>
vars == <<rec, made, manifest, sealed, up, plog, ptrunc, fenced, st, seen, ep, base, log, tail, wm, upseen, issued, ackedIn, proven, born, wipes, promised, lease, staleRead>>

Mine(x) == [node |-> NodeOf[x], epoch |-> ep[x]]
NoTail == [e \in Epochs |-> [ents |-> <<>>, from |-> 0]]
\* A fence's answer from one peer: did we get one, the peer's `born`, and
\* its tail of every epoch.
NoReply == [got |-> FALSE, born |-> 0, ts |-> NoTail]
\* The last epoch before e that served (has a manifest), or 0.
Last(e) == LET qs == {q \in 1..(e - 1) : made[q]} IN IF qs = {} THEN 0 ELSE Max(qs)

\* Every action below that doesn't name the lease variables leaves them be.
Reset(x) ==
    /\ st' = [st EXCEPT ![x] = "idle"]
    /\ log' = [log EXCEPT ![x] = <<>>]
    /\ wm' = [wm EXCEPT ![x] = 0]

Init ==
    /\ rec = NoRec
    /\ made = [e \in Epochs |-> FALSE]
    /\ manifest = [e \in Epochs |-> <<>>]
    /\ sealed = [e \in Epochs |-> FALSE]
    /\ up = [e \in Epochs |-> <<>>]
    /\ plog = [p \in Peers |-> [e \in Epochs |-> <<>>]]
    /\ ptrunc = [p \in Peers |-> [e \in Epochs |-> 0]]
    /\ fenced = [p \in Peers |-> 0]
    /\ born = [p \in Peers |-> 0]
    /\ wipes = 0
    /\ st = [x \in Procs |-> "idle"]
    /\ seen = [x \in Procs |-> NoRec]
    /\ ep = [x \in Procs |-> 0]
    /\ base = [x \in Procs |-> <<>>]
    /\ log = [x \in Procs |-> <<>>]
    \* The answers this attempt's fences got, per peer.
    /\ tail = [x \in Procs |-> [p \in Peers |-> NoReply]]
    /\ wm = [x \in Procs |-> 0]
    /\ upseen = [x \in Procs |-> <<>>]
    /\ issued = {}
    /\ ackedIn = [w \in Writes |-> 0]
    /\ proven = [e \in Epochs |-> <<>>]
    /\ promised = [p \in Peers |-> 0]
    /\ lease = [x \in Procs |-> FALSE]
    /\ staleRead = FALSE

\* ---- 1. claim ----
ReadRec(x) ==
    /\ st[x] = "idle"
    /\ seen' = [seen EXCEPT ![x] = rec]
    /\ st' = [st EXCEPT ![x] = "read"]
    /\ UNCHANGED <<rec, made, manifest, sealed, up, plog, ptrunc, fenced, ep, base, log, tail, wm, upseen, issued, ackedIn, proven, born, wipes, promised, lease, staleRead>>

Cas(x) ==
    /\ st[x] = "read"
    /\ seen[x].epoch < MaxEpoch
    /\ IF rec = seen[x]
       THEN /\ rec' = [node |-> NodeOf[x], epoch |-> seen[x].epoch + 1]
            /\ ep' = [ep EXCEPT ![x] = seen[x].epoch + 1]
            /\ st' = [st EXCEPT ![x] = "fence"]
       ELSE /\ st' = [st EXCEPT ![x] = "idle"]
            /\ UNCHANGED <<rec, ep>>
    /\ tail' = [tail EXCEPT ![x] = [p \in Peers |-> NoReply]]
    \* Wrong on purpose: read the bucket before fencing.
    /\ upseen' = IF BUG_LOAD_BEFORE_FENCE THEN [upseen EXCEPT ![x] = LET q == Last(seen[x].epoch + 1) IN IF q = 0 THEN <<>> ELSE up[q]] ELSE upseen
    /\ UNCHANGED <<made, manifest, sealed, up, plog, ptrunc, fenced, seen, base, log, wm, issued, ackedIn, proven, born, wipes, promised, lease, staleRead>>

\* ---- 2. fence peers, read their tails ----
\* Must we fence? Not if we took over from no epoch, or from a sealed one.
MustFence(x) ==
    IF BUG_FENCE_IF_MANIFEST
    THEN LET q == Last(ep[x]) IN IF q = 0 THEN FALSE ELSE ~sealed[q]
    ELSE IF seen[x].epoch = 0 THEN FALSE ELSE ~sealed[seen[x].epoch]

\* One peer at a time; more may follow before we load (if the first can't
\* speak for the epoch we need, we fence another and list again).
Fence(x, p) ==
    /\ st[x] \in {"fence", "load"}
    /\ MustFence(x)
    \* A promise to an older epoch: wait it out.
    /\ BUG_LEASE_NO_PROMISE \/ promised[p] = 0 \/ promised[p] >= ep[x]
    /\ fenced' = IF BUG_NO_FENCE THEN fenced ELSE [fenced EXCEPT ![p] = Max({@, ep[x] - 1})]
    \* Every epoch's tail: we don't know yet which epoch we'll load from.
    /\ tail' = [tail EXCEPT ![x][p] = [got |-> TRUE, born |-> born[p], ts |-> [e \in Epochs |->
                   [ents |-> SubSeq(plog[p][e], ptrunc[p][e] + 1, Len(plog[p][e])), from |-> ptrunc[p][e]]]]]
    /\ st' = [st EXCEPT ![x] = "load"]
    /\ UNCHANGED <<rec, made, manifest, sealed, up, plog, ptrunc, seen, ep, base, log, wm, upseen, issued, ackedIn, proven, born, wipes, promised, lease, staleRead>>

\* Nothing to fence: no epoch served before, or it was sealed.
NoFence(x) ==
    /\ st[x] = "fence"
    /\ ~MustFence(x)
    /\ st' = [st EXCEPT ![x] = "load"]
    /\ UNCHANGED <<tail, rec, made, manifest, sealed, up, plog, ptrunc, fenced, seen, ep, base, log, wm, upseen, issued, ackedIn, proven, born, wipes, promised, lease, staleRead>>

\* List the bucket. If it shows a newer epoch's manifest, we're stale: stop
\* (the peers may have dropped what we'd need). Else q is the last epoch
\* with a manifest, as the listing shows it now (after the fence). The
\* history: q's manifest, then q's entries from the bucket, then whatever
\* q's tail has past that. Both are prefixes of q's owner's log.
Stale(x) ==
    /\ st[x] = "load"
    /\ ~BUG_LATE_MANIFEST
    /\ \E e \in Epochs : e > ep[x] /\ made[e]
    /\ Reset(x)
    /\ UNCHANGED <<rec, made, manifest, sealed, up, plog, ptrunc, fenced, seen, ep, base, tail, upseen, issued, ackedIn, proven, born, wipes, promised, lease, staleRead>>

\* Can peer p's answer speak for epoch q? A peer that lost its disk knows only
\* the epochs that began after it came back (after the epoch it relearned):
\* it saw all of those. Of older ones it may hold a partial copy, rebuilt
\* while their owner re-sent entries. A peer that lost nothing knows all.
Usable(x, p, q) ==
    /\ tail[x][p].got
    /\ BUG_TRUST_WIPED \/ q > tail[x][p].born

LoadFrom(x, t) ==
    /\ st[x] = "load"
    /\ BUG_LATE_MANIFEST \/ ~\E e \in Epochs : e > ep[x] /\ made[e]
    /\ LET q == Last(ep[x])
           u == IF BUG_LOAD_BEFORE_FENCE THEN upseen[x] ELSE IF q = 0 THEN <<>> ELSE up[q]
           tend == t.from + Len(t.ents)
           more == IF tend > Len(u) /\ t.from <= Len(u) THEN SubSeq(t.ents, Len(u) - t.from + 1, Len(t.ents)) ELSE <<>>
       IN base' = [base EXCEPT ![x] = (IF q = 0 THEN <<>> ELSE manifest[q]) \o u \o more]
    /\ st' = [st EXCEPT ![x] = "manifest"]
    /\ UNCHANGED <<rec, made, manifest, sealed, up, plog, ptrunc, fenced, seen, ep, log, tail, wm, upseen, issued, ackedIn, proven, born, wipes, promised, lease, staleRead>>

Load(x) ==
    LET q == Last(ep[x]) IN
    \/ ~MustFence(x) /\ LoadFrom(x, [ents |-> <<>>, from |-> 0])
    \/ \E p \in Peers :
          /\ MustFence(x)
          /\ tail[x][p].got
          /\ IF q = 0 THEN TRUE ELSE Usable(x, p, q)
          /\ LoadFrom(x, IF q = 0 THEN [ents |-> <<>>, from |-> 0] ELSE tail[x][p].ts[q])

WriteManifest(x) ==
    /\ st[x] = "manifest"
    /\ made' = [made EXCEPT ![ep[x]] = TRUE]
    /\ manifest' = [manifest EXCEPT ![ep[x]] = base[x]]
    /\ st' = [st EXCEPT ![x] = "serving"]
    /\ UNCHANGED <<rec, sealed, up, plog, ptrunc, fenced, seen, ep, base, log, tail, wm, upseen, issued, ackedIn, proven, born, wipes, promised, lease, staleRead>>

\* ---- 3. serve ----
Exec(x, w) ==
    /\ st[x] = "serving"
    /\ w \notin issued
    /\ log' = [log EXCEPT ![x] = Append(@, w)]
    /\ issued' = issued \cup {w}
    /\ UNCHANGED <<rec, made, manifest, sealed, up, plog, ptrunc, fenced, st, seen, ep, base, tail, wm, upseen, ackedIn, proven, born, wipes, promised, lease, staleRead>>

\* Send peer p the next entry. A fenced peer refuses, and we step down. A
\* peer that takes an entry of epoch E drops every older epoch's log: E has
\* its manifest, and that holds everything older.
Send(x, p) ==
    /\ st[x] = "serving"
    /\ Len(plog[p][ep[x]]) < Len(log[x])
    /\ IF fenced[p] < ep[x]
       THEN /\ plog' = [plog EXCEPT ![p][ep[x]] = Append(@, log[x][Len(@) + 1])]
            /\ ptrunc' = [ptrunc EXCEPT ![p] = [e \in Epochs |-> IF e < ep[x] THEN Len(plog[p][e]) ELSE @[e]]]
            /\ promised' = [promised EXCEPT ![p] = Max({@, ep[x]})]
            /\ UNCHANGED <<st, log, wm, lease>>
       ELSE /\ Reset(x)
            /\ lease' = [lease EXCEPT ![x] = FALSE]
            /\ UNCHANGED <<plog, ptrunc, promised>>
    /\ UNCHANGED <<rec, made, manifest, sealed, up, fenced, seen, ep, base, tail, upseen, issued, ackedIn, proven, born, wipes, staleRead>>

\* Acknowledge the entries every peer has (or, wrongly, any one peer).
Ack(x) ==
    /\ st[x] = "serving"
    /\ LET lens == {Len(plog[p][ep[x]]) : p \in Peers}
           k == IF BUG_ACK_ONE THEN Max(lens) ELSE Min(lens)
       IN /\ k > 0
          /\ ackedIn' = [w \in Writes |-> IF w \in Range(Take(log[x], k)) /\ ackedIn[w] = 0 THEN ep[x] ELSE ackedIn[w]]
          /\ proven' = [proven EXCEPT ![ep[x]] = IF Len(base[x]) + k > Len(@) THEN base[x] \o Take(log[x], k) ELSE @]
    /\ UNCHANGED <<rec, made, manifest, sealed, up, plog, ptrunc, fenced, st, seen, ep, base, log, tail, wm, upseen, issued, born, wipes, promised, lease, staleRead>>

\* Or, when a peer is slow or down: acknowledge what the bucket already has,
\* after re-reading rec (D2's proof).
AckBucket(x) ==
    /\ st[x] = "serving"
    /\ BUG_ACK_NO_PROOF \/ rec = Mine(x)
    /\ LET k == Len(up[ep[x]])
       IN /\ k > 0
          /\ ackedIn' = [w \in Writes |-> IF w \in Range(Take(log[x], k)) /\ ackedIn[w] = 0 THEN ep[x] ELSE ackedIn[w]]
          /\ proven' = [proven EXCEPT ![ep[x]] = IF Len(base[x]) + k > Len(@) THEN base[x] \o Take(log[x], k) ELSE @]
    /\ UNCHANGED <<rec, made, manifest, sealed, up, plog, ptrunc, fenced, st, seen, ep, base, log, tail, wm, upseen, issued, born, wipes, promised, lease, staleRead>>

\* ---- 4. upload to the bucket, then let peers drop it ----
Upload(x) ==
    /\ st[x] = "serving"
    /\ Len(up[ep[x]]) < Len(log[x])
    /\ up' = [up EXCEPT ![ep[x]] = Take(log[x], Len(@) + 1)]
    /\ wm' = [wm EXCEPT ![x] = Len(up[ep[x]]) + 1]
    /\ UNCHANGED <<rec, made, manifest, sealed, plog, ptrunc, fenced, st, seen, ep, base, log, tail, upseen, issued, ackedIn, proven, born, wipes, promised, lease, staleRead>>

Trunc(x, p) ==
    /\ st[x] = "serving"
    /\ LET n == IF BUG_TRUNC_EARLY THEN Len(plog[p][ep[x]]) ELSE Min({wm[x], Len(plog[p][ep[x]])})
       IN /\ n > ptrunc[p][ep[x]]
          /\ ptrunc' = [ptrunc EXCEPT ![p][ep[x]] = n]
    /\ UNCHANGED <<rec, made, manifest, sealed, up, plog, fenced, st, seen, ep, base, log, tail, wm, upseen, issued, ackedIn, proven, born, wipes, promised, lease, staleRead>>

\* ---- 5. seal: upload everything, mark the epoch, step down ----
Seal(x) ==
    /\ st[x] = "serving"
    /\ BUG_SEAL_EARLY \/ Len(up[ep[x]]) = Len(log[x])
    /\ sealed' = [sealed EXCEPT ![ep[x]] = TRUE]
    /\ Reset(x)
    /\ lease' = [lease EXCEPT ![x] = FALSE]
    /\ UNCHANGED <<rec, made, manifest, up, plog, ptrunc, fenced, seen, ep, base, tail, upseen, issued, ackedIn, proven, born, wipes, promised, staleRead>>

\* A peer loses its disk and comes back empty. Before it answers anything,
\* it reads the ownership record: it refuses every epoch older than the
\* current one (the fence it lost), and knows only the epochs after it.
\* (A real peer reads the record later, on first touch: a newer record
\* only refuses more and knows less.)
Wipe(p) ==
    /\ wipes < MaxWipes
    /\ wipes' = wipes + 1
    /\ born' = [born EXCEPT ![p] = rec.epoch]
    /\ plog' = [plog EXCEPT ![p] = [e \in Epochs |-> <<>>]]
    /\ ptrunc' = [ptrunc EXCEPT ![p] = [e \in Epochs |-> 0]]
    /\ fenced' = [fenced EXCEPT ![p] = IF BUG_WIPE_FORGETS_FENCE \/ rec.epoch = 0 THEN 0 ELSE rec.epoch - 1]
    \* (It keeps its promise: a restarted store holds off fences a lease first.)
    /\ UNCHANGED <<rec, made, manifest, sealed, up, st, seen, ep, base, log, tail, wm, upseen, issued, ackedIn, proven, promised, lease, staleRead>>

Crash(x) ==
    /\ st[x] # "idle"
    /\ Reset(x)
    /\ lease' = [lease EXCEPT ![x] = FALSE]
    /\ UNCHANGED <<rec, made, manifest, sealed, up, plog, ptrunc, fenced, seen, ep, base, tail, upseen, issued, ackedIn, proven, born, wipes, promised, staleRead>>

\* ---- read leases ----
\* An empty round: the peer promises again (or refuses, if fenced).
Renew(x, p) ==
    /\ st[x] = "serving"
    /\ IF fenced[p] < ep[x]
       THEN /\ promised' = [promised EXCEPT ![p] = Max({@, ep[x]})]
            /\ UNCHANGED <<st, log, wm, lease>>
       ELSE /\ Reset(x)
            /\ lease' = [lease EXCEPT ![x] = FALSE]
            /\ UNCHANGED promised
    /\ UNCHANGED <<rec, made, manifest, sealed, up, plog, ptrunc, fenced, seen, ep, base, tail, upseen, issued, ackedIn, proven, born, wipes, staleRead>>

\* Every peer has promised our epoch: we hold a lease.
GetLease(x) ==
    /\ st[x] = "serving"
    /\ \A p \in Peers : promised[p] = ep[x]
    /\ lease' = [lease EXCEPT ![x] = TRUE]
    /\ UNCHANGED <<rec, made, manifest, sealed, up, plog, ptrunc, fenced, st, seen, ep, base, log, tail, wm, upseen, issued, ackedIn, proven, born, wipes, promised, staleRead>>

\* Our lease runs out (any time: our clock says so).
LeaseEnd(x) ==
    /\ lease[x]
    /\ lease' = [lease EXCEPT ![x] = FALSE]
    /\ UNCHANGED <<rec, made, manifest, sealed, up, plog, ptrunc, fenced, st, seen, ep, base, log, tail, wm, upseen, issued, ackedIn, proven, born, wipes, promised, staleRead>>

\* A peer's promise lapses: only once no owner of that epoch still trusts
\* it (the clock assumption), unless the owner trusts too long.
Expire(p) ==
    /\ promised[p] > 0
    /\ BUG_LEASE_OVERTRUST \/ ~\E x \in Procs : lease[x] /\ ep[x] = promised[p]
    /\ promised' = [promised EXCEPT ![p] = 0]
    /\ UNCHANGED <<rec, made, manifest, sealed, up, plog, ptrunc, fenced, st, seen, ep, base, log, tail, wm, upseen, issued, ackedIn, proven, born, wipes, lease, staleRead>>

\* A read under the lease, from what we have, with no round: it must see
\* every acknowledged write.
LeaseRead(x) ==
    /\ st[x] = "serving"
    /\ lease[x]
    /\ staleRead' = (staleRead \/ \E w \in Writes : ackedIn[w] > 0 /\ w \notin Range(base[x] \o log[x]))
    /\ UNCHANGED <<rec, made, manifest, sealed, up, plog, ptrunc, fenced, st, seen, ep, base, log, tail, wm, upseen, issued, ackedIn, proven, born, wipes, promised, lease>>

Next ==
    \/ \E x \in Procs : ReadRec(x) \/ Cas(x) \/ NoFence(x) \/ Stale(x) \/ Load(x) \/ WriteManifest(x) \/ Ack(x) \/ AckBucket(x) \/ Upload(x) \/ Seal(x) \/ Crash(x)
                        \/ GetLease(x) \/ LeaseEnd(x) \/ LeaseRead(x)
    \/ \E x \in Procs, p \in Peers : Fence(x, p) \/ Send(x, p) \/ Trunc(x, p) \/ Renew(x, p)
    \/ \E p \in Peers : Wipe(p) \/ Expire(p)
    \/ \E x \in Procs, w \in Writes : Exec(x, w)

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------
OneWriterPerEpoch ==
    \A x, y \in Procs : (x # y /\ st[x] = "serving" /\ st[y] = "serving") => ep[x] # ep[y]

NoLostAckedWrite ==
    \A w \in Writes, e \in Epochs :
        (ackedIn[w] > 0 /\ e > ackedIn[w] /\ made[e]) => w \in Range(manifest[e])

NoHistoryRewrite ==
    \A e1, e2 \in Epochs : (e1 < e2 /\ made[e2]) => IsPrefix(proven[e1], manifest[e2])

\* A read answered under a lease saw every acknowledged write.
NoStaleRead == ~staleRead

=============================================================================
