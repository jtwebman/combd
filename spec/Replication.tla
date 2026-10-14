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
(*      ours from then on, and returns its tail of q, the last epoch with  *)
(*      a manifest, in the same call. Only then list the bucket. History = *)
(*      manifest[q] + q's uploaded entries + what the tail adds. Write it  *)
(*      as our manifest.                                                   *)
(*                                                                         *)
(*      Fence the epoch we took over from even if it never wrote a         *)
(*      manifest: its owner may write one late and start serving, and with *)
(*      no fence nothing would stop it. (TLC found that one.) An object's  *)
(*      peer set stays the same across epochs, so one fence covers every   *)
(*      older epoch. After a sealed epoch, skipping is safe: whoever       *)
(*      started that epoch fenced everything older.                        *)
(*   3. Serve. Send each entry, in order, to every peer; a peer takes it    *)
(*      only if not fenced. Acknowledge once EVERY peer has it.            *)
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
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS Procs, NodeOf, Peers, Writes, MaxEpoch,
          BUG_ACK_ONE,           \* acknowledge when one peer has it, not all
          BUG_NO_FENCE,          \* read a peer's tail without fencing it
          BUG_LOAD_BEFORE_FENCE, \* list the bucket before fencing
          BUG_TRUNC_EARLY,       \* peers drop entries before they're uploaded
          BUG_SEAL_EARLY,        \* seal before everything is uploaded
          BUG_FENCE_IF_MANIFEST  \* fence only if an epoch has a manifest (TLC's find)

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
    st, seen, ep, base, log, tail, wm, upseen, qf, \* each process
    issued, ackedIn, proven \* history, for the invariants

vars == <<rec, made, manifest, sealed, up, plog, ptrunc, fenced, st, seen, ep, base, log, tail, wm, upseen, qf, issued, ackedIn, proven>>

Mine(x) == [node |-> NodeOf[x], epoch |-> ep[x]]
\* The last epoch before e that served (has a manifest), or 0.
Last(e) == LET qs == {q \in 1..(e - 1) : made[q]} IN IF qs = {} THEN 0 ELSE Max(qs)

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
    /\ st = [x \in Procs |-> "idle"]
    /\ seen = [x \in Procs |-> NoRec]
    /\ ep = [x \in Procs |-> 0]
    /\ base = [x \in Procs |-> <<>>]
    /\ log = [x \in Procs |-> <<>>]
    \* The tail a fence returned: (entries, index of its first entry - 1).
    /\ tail = [x \in Procs |-> [ents |-> <<>>, from |-> 0]]
    /\ wm = [x \in Procs |-> 0]
    /\ upseen = [x \in Procs |-> <<>>]
    \* The epoch to load from, fixed when fencing.
    /\ qf = [x \in Procs |-> 0]
    /\ issued = {}
    /\ ackedIn = [w \in Writes |-> 0]
    /\ proven = [e \in Epochs |-> <<>>]

\* ---- 1. claim ----
ReadRec(x) ==
    /\ st[x] = "idle"
    /\ seen' = [seen EXCEPT ![x] = rec]
    /\ st' = [st EXCEPT ![x] = "read"]
    /\ UNCHANGED <<rec, made, manifest, sealed, up, plog, ptrunc, fenced, ep, base, log, tail, wm, upseen, qf, issued, ackedIn, proven>>

Cas(x) ==
    /\ st[x] = "read"
    /\ seen[x].epoch < MaxEpoch
    /\ IF rec = seen[x]
       THEN /\ rec' = [node |-> NodeOf[x], epoch |-> seen[x].epoch + 1]
            /\ ep' = [ep EXCEPT ![x] = seen[x].epoch + 1]
            /\ st' = [st EXCEPT ![x] = "fence"]
       ELSE /\ st' = [st EXCEPT ![x] = "idle"]
            /\ UNCHANGED <<rec, ep>>
    \* Wrong on purpose: read the bucket before fencing.
    /\ upseen' = IF BUG_LOAD_BEFORE_FENCE THEN [upseen EXCEPT ![x] = LET q == Last(seen[x].epoch + 1) IN IF q = 0 THEN <<>> ELSE up[q]] ELSE upseen
    /\ UNCHANGED <<made, manifest, sealed, up, plog, ptrunc, fenced, seen, base, log, tail, wm, qf, issued, ackedIn, proven>>

\* ---- 2. fence one peer of the last epoch that served, read its tail ----
\* Must we fence? Not if we took over from no epoch, or from a sealed one.
MustFence(x) ==
    IF BUG_FENCE_IF_MANIFEST
    THEN LET q == Last(ep[x]) IN IF q = 0 THEN FALSE ELSE ~sealed[q]
    ELSE IF seen[x].epoch = 0 THEN FALSE ELSE ~sealed[seen[x].epoch]

Fence(x, p) ==
    /\ st[x] = "fence"
    /\ MustFence(x)
    /\ LET q == Last(ep[x]) IN
       /\ fenced' = IF BUG_NO_FENCE THEN fenced ELSE [fenced EXCEPT ![p] = Max({@, ep[x] - 1})]
       /\ tail' = [tail EXCEPT ![x] = IF q = 0 THEN [ents |-> <<>>, from |-> 0]
                                      ELSE [ents |-> SubSeq(plog[p][q], ptrunc[p][q] + 1, Len(plog[p][q])), from |-> ptrunc[p][q]]]
       /\ qf' = [qf EXCEPT ![x] = q]
    /\ st' = [st EXCEPT ![x] = "load"]
    /\ UNCHANGED <<rec, made, manifest, sealed, up, plog, ptrunc, seen, ep, base, log, wm, upseen, issued, ackedIn, proven>>

\* Nothing to fence: no epoch served before, or it was sealed.
NoFence(x) ==
    /\ st[x] = "fence"
    /\ ~MustFence(x)
    /\ tail' = [tail EXCEPT ![x] = [ents |-> <<>>, from |-> 0]]
    /\ qf' = [qf EXCEPT ![x] = Last(ep[x])]
    /\ st' = [st EXCEPT ![x] = "load"]
    /\ UNCHANGED <<rec, made, manifest, sealed, up, plog, ptrunc, fenced, seen, ep, base, log, wm, upseen, issued, ackedIn, proven>>

\* The history: q's manifest, then q's entries from the bucket, then whatever
\* the tail has past that. Both are prefixes of q's owner's log.
Load(x) ==
    /\ st[x] = "load"
    /\ LET q == qf[x]
           u == IF BUG_LOAD_BEFORE_FENCE THEN upseen[x] ELSE IF q = 0 THEN <<>> ELSE up[q]
           t == tail[x]
           tend == t.from + Len(t.ents)
           more == IF tend > Len(u) /\ t.from <= Len(u) THEN SubSeq(t.ents, Len(u) - t.from + 1, Len(t.ents)) ELSE <<>>
       IN base' = [base EXCEPT ![x] = (IF q = 0 THEN <<>> ELSE manifest[q]) \o u \o more]
    /\ st' = [st EXCEPT ![x] = "manifest"]
    /\ UNCHANGED <<rec, made, manifest, sealed, up, plog, ptrunc, fenced, seen, ep, log, tail, wm, upseen, qf, issued, ackedIn, proven>>

WriteManifest(x) ==
    /\ st[x] = "manifest"
    /\ made' = [made EXCEPT ![ep[x]] = TRUE]
    /\ manifest' = [manifest EXCEPT ![ep[x]] = base[x]]
    /\ st' = [st EXCEPT ![x] = "serving"]
    /\ UNCHANGED <<rec, sealed, up, plog, ptrunc, fenced, seen, ep, base, log, tail, wm, upseen, qf, issued, ackedIn, proven>>

\* ---- 3. serve ----
Exec(x, w) ==
    /\ st[x] = "serving"
    /\ w \notin issued
    /\ log' = [log EXCEPT ![x] = Append(@, w)]
    /\ issued' = issued \cup {w}
    /\ UNCHANGED <<rec, made, manifest, sealed, up, plog, ptrunc, fenced, st, seen, ep, base, tail, wm, upseen, qf, ackedIn, proven>>

\* Send peer p the next entry. A fenced peer refuses, and we step down.
Send(x, p) ==
    /\ st[x] = "serving"
    /\ Len(plog[p][ep[x]]) < Len(log[x])
    /\ IF fenced[p] < ep[x]
       THEN /\ plog' = [plog EXCEPT ![p][ep[x]] = Append(@, log[x][Len(@) + 1])]
            /\ UNCHANGED <<st, log, wm>>
       ELSE /\ Reset(x)
            /\ UNCHANGED plog
    /\ UNCHANGED <<rec, made, manifest, sealed, up, ptrunc, fenced, seen, ep, base, tail, upseen, qf, issued, ackedIn, proven>>

\* Acknowledge the entries every peer has (or, wrongly, any one peer).
Ack(x) ==
    /\ st[x] = "serving"
    /\ LET lens == {Len(plog[p][ep[x]]) : p \in Peers}
           k == IF BUG_ACK_ONE THEN Max(lens) ELSE Min(lens)
       IN /\ k > 0
          /\ ackedIn' = [w \in Writes |-> IF w \in Range(Take(log[x], k)) /\ ackedIn[w] = 0 THEN ep[x] ELSE ackedIn[w]]
          /\ proven' = [proven EXCEPT ![ep[x]] = IF Len(base[x]) + k > Len(@) THEN base[x] \o Take(log[x], k) ELSE @]
    /\ UNCHANGED <<rec, made, manifest, sealed, up, plog, ptrunc, fenced, st, seen, ep, base, log, tail, wm, upseen, qf, issued>>

\* ---- 4. upload to the bucket, then let peers drop it ----
Upload(x) ==
    /\ st[x] = "serving"
    /\ Len(up[ep[x]]) < Len(log[x])
    /\ up' = [up EXCEPT ![ep[x]] = Take(log[x], Len(@) + 1)]
    /\ wm' = [wm EXCEPT ![x] = Len(up[ep[x]]) + 1]
    /\ UNCHANGED <<rec, made, manifest, sealed, plog, ptrunc, fenced, st, seen, ep, base, log, tail, upseen, qf, issued, ackedIn, proven>>

Trunc(x, p) ==
    /\ st[x] = "serving"
    /\ LET n == IF BUG_TRUNC_EARLY THEN Len(plog[p][ep[x]]) ELSE Min({wm[x], Len(plog[p][ep[x]])})
       IN /\ n > ptrunc[p][ep[x]]
          /\ ptrunc' = [ptrunc EXCEPT ![p][ep[x]] = n]
    /\ UNCHANGED <<rec, made, manifest, sealed, up, plog, fenced, st, seen, ep, base, log, tail, wm, upseen, qf, issued, ackedIn, proven>>

\* ---- 5. seal: upload everything, mark the epoch, step down ----
Seal(x) ==
    /\ st[x] = "serving"
    /\ BUG_SEAL_EARLY \/ Len(up[ep[x]]) = Len(log[x])
    /\ sealed' = [sealed EXCEPT ![ep[x]] = TRUE]
    /\ Reset(x)
    /\ UNCHANGED <<rec, made, manifest, up, plog, ptrunc, fenced, seen, ep, base, tail, upseen, qf, issued, ackedIn, proven>>

Crash(x) ==
    /\ st[x] # "idle"
    /\ Reset(x)
    /\ UNCHANGED <<rec, made, manifest, sealed, up, plog, ptrunc, fenced, seen, ep, base, tail, upseen, qf, issued, ackedIn, proven>>

Next ==
    \/ \E x \in Procs : ReadRec(x) \/ Cas(x) \/ NoFence(x) \/ Load(x) \/ WriteManifest(x) \/ Ack(x) \/ Upload(x) \/ Seal(x) \/ Crash(x)
    \/ \E x \in Procs, p \in Peers : Fence(x, p) \/ Send(x, p) \/ Trunc(x, p)
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

=============================================================================
