---------------------------- MODULE Ownership ----------------------------
(***************************************************************************)
(* combd D2: who owns a Durable Object, with the bucket as the only        *)
(* coordinator. One object, a few processes, a few writes.                 *)
(*                                                                         *)
(* The bucket holds:                                                       *)
(*   rec          the ownership record [node, epoch], changed only by      *)
(*                compare-and-swap (S3 conditional writes)                 *)
(*   segs[e]      data segments written in epoch e, in order               *)
(*   manifest[e]  the history the owner of epoch e started from            *)
(*                                                                         *)
(* The protocol:                                                           *)
(*   1. Claim: read rec, then CAS it to [me, epoch + 1]. Every activation  *)
(*      takes a new epoch, even a same-node wake. A lost CAS reply is      *)
(*      followed by a read-back, never a blind retry.                      *)
(*   2. Load, after the CAS: the history of the last epoch with a          *)
(*      manifest, plus that epoch's segments. Write it as our manifest.    *)
(*   3. Serve: run writes, upload them as a segment, then prove: re-read   *)
(*      rec, and acknowledge only if it still says [me, my epoch].         *)
(*                                                                         *)
(* Clocks and leases aren't modeled: leases only stop us from taking over  *)
(* a healthy owner. Safety must hold with any clocks, so any process may   *)
(* claim at any time.                                                      *)
(*                                                                         *)
(* Processes, not nodes: two processes can share a node name, which is a   *)
(* frozen old process and the one its supervisor restarted.                *)
(*                                                                         *)
(* The BUG_ constants switch on known-bad versions. Each must break an     *)
(* invariant, which proves the checker can see that kind of bug.           *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS Procs, NodeOf, Writes, MaxEpoch,
          BUG_SKIP_PROOF,       \* acknowledge on upload, without re-reading rec
          BUG_LOAD_BEFORE_CAS,  \* read the history before winning the CAS
          BUG_NO_MANIFEST,      \* rebuild history from every segment in the bucket
          BUG_REUSE_EPOCH       \* a same-node wake keeps its old epoch

Epochs == 1..MaxEpoch
NoRec == [node |-> "none", epoch |-> 0]

VARIABLES
    rec, segs, made, manifest, inflight,     \* the bucket, and uploads on the way
    st, seen, ep, base, pending, unproved,   \* each process
    issued, ackedIn, proven                  \* history, for the invariants

bucket == <<rec, segs, made, manifest, inflight>>
local == <<st, seen, ep, base, pending, unproved>>
ghost == <<issued, ackedIn, proven>>
vars == <<bucket, local, ghost>>

Flatten(s) == LET F[i \in 0..Len(s)] == IF i = 0 THEN <<>> ELSE F[i - 1] \o s[i]
              IN F[Len(s)]
Range(s) == {s[i] : i \in DOMAIN s}
IsPrefix(a, b) == Len(a) <= Len(b) /\ SubSeq(b, 1, Len(a)) = a
Max(S) == CHOOSE x \in S : \A y \in S : x >= y
Mine(p) == [node |-> NodeOf[p], epoch |-> ep[p]]

\* The history an owner of epoch e starts from.
History(e) ==
    IF BUG_NO_MANIFEST
    THEN Flatten([i \in 1..(e - 1) |-> Flatten(segs[i])])
    ELSE LET ps == {q \in 1..(e - 1) : made[q]}
         IN IF ps = {} THEN <<>>
            ELSE LET q == Max(ps) IN manifest[q] \o Flatten(segs[q])

Reset(p) ==
    /\ st' = [st EXCEPT ![p] = "idle"]
    /\ pending' = [pending EXCEPT ![p] = <<>>]
    /\ unproved' = [unproved EXCEPT ![p] = {}]

Init ==
    /\ rec = NoRec
    /\ segs = [e \in Epochs |-> <<>>]
    /\ made = [e \in Epochs |-> FALSE]
    /\ manifest = [e \in Epochs |-> <<>>]
    /\ inflight = {}
    /\ st = [p \in Procs |-> "idle"]
    /\ seen = [p \in Procs |-> NoRec]
    /\ ep = [p \in Procs |-> 0]
    /\ base = [p \in Procs |-> <<>>]
    /\ pending = [p \in Procs |-> <<>>]
    /\ unproved = [p \in Procs |-> {}]
    /\ issued = {}
    /\ ackedIn = [w \in Writes |-> 0]
    /\ proven = [e \in Epochs |-> <<>>]

\* 1. Claim.
ReadRec(p) ==
    /\ st[p] = "idle"
    /\ seen' = [seen EXCEPT ![p] = rec]
    /\ st' = [st EXCEPT ![p] = "read"]
    /\ base' = IF BUG_LOAD_BEFORE_CAS /\ rec.epoch < MaxEpoch
               THEN [base EXCEPT ![p] = History(rec.epoch + 1)] ELSE base
    /\ UNCHANGED <<bucket, ep, pending, unproved, ghost>>

NextEpoch(p) ==
    IF BUG_REUSE_EPOCH /\ seen[p].node = NodeOf[p] THEN seen[p].epoch ELSE seen[p].epoch + 1

Cas(p) ==
    /\ st[p] = "read"
    /\ NextEpoch(p) \in Epochs
    /\ IF rec = seen[p]
       THEN /\ rec' = [node |-> NodeOf[p], epoch |-> NextEpoch(p)]
            /\ ep' = [ep EXCEPT ![p] = NextEpoch(p)]
            \* The reply may be lost: then we don't know if we won.
            /\ \E lost \in BOOLEAN : st' = [st EXCEPT ![p] = IF lost THEN "unsure" ELSE "loading"]
       ELSE /\ st' = [st EXCEPT ![p] = "idle"]
            /\ UNCHANGED <<rec, ep>>
    /\ UNCHANGED <<segs, made, manifest, inflight, seen, base, pending, unproved, ghost>>

ReadBack(p) ==
    /\ st[p] = "unsure"
    /\ st' = [st EXCEPT ![p] = IF rec = Mine(p) THEN "loading" ELSE "idle"]
    /\ UNCHANGED <<bucket, seen, ep, base, pending, unproved, ghost>>

\* 2. Load, then write the manifest.
Load(p) ==
    /\ st[p] = "loading"
    /\ base' = IF BUG_LOAD_BEFORE_CAS THEN base ELSE [base EXCEPT ![p] = History(ep[p])]
    /\ st' = [st EXCEPT ![p] = "manifest"]
    /\ UNCHANGED <<bucket, seen, ep, pending, unproved, ghost>>

WriteManifest(p) ==
    /\ st[p] = "manifest"
    /\ made' = [made EXCEPT ![ep[p]] = TRUE]
    /\ manifest' = [manifest EXCEPT ![ep[p]] = base[p]]
    /\ st' = [st EXCEPT ![p] = "serving"]
    /\ UNCHANGED <<rec, segs, inflight, seen, ep, base, pending, unproved, ghost>>

\* 3. Serve.
Exec(p, w) ==
    /\ st[p] = "serving"
    /\ w \notin issued
    /\ pending' = [pending EXCEPT ![p] = Append(@, w)]
    /\ issued' = issued \cup {w}
    /\ UNCHANGED <<bucket, st, seen, ep, base, unproved, ackedIn, proven>>

\* Send a segment. One upload at a time per process; it lands later, maybe
\* after the process crashed or lost the object.
Upload(p) ==
    /\ st[p] = "serving"
    /\ pending[p] # <<>>
    /\ ~\E m \in inflight : m.p = p
    /\ inflight' = inflight \cup {[p |-> p, e |-> ep[p], seg |-> pending[p]]}
    /\ pending' = [pending EXCEPT ![p] = <<>>]
    /\ st' = [st EXCEPT ![p] = "uploading"]
    /\ ackedIn' = IF BUG_SKIP_PROOF
                  THEN [w \in Writes |-> IF w \in Range(pending[p]) THEN ep[p] ELSE ackedIn[w]]
                  ELSE ackedIn
    /\ UNCHANGED <<rec, segs, made, manifest, seen, ep, base, unproved, issued, proven>>

Land(m) ==
    /\ m \in inflight
    /\ inflight' = inflight \ {m}
    /\ segs' = [segs EXCEPT ![m.e] = Append(@, m.seg)]
    \* The uploader hears back only if it's still waiting for this upload;
    \* the reply may be lost, and then it gives the object up.
    /\ IF st[m.p] = "uploading" /\ ep[m.p] = m.e
       THEN \E lost \in BOOLEAN :
              IF lost THEN Reset(m.p)
              ELSE /\ st' = [st EXCEPT ![m.p] = "serving"]
                   /\ unproved' = [unproved EXCEPT ![m.p] = @ \cup Range(m.seg)]
                   /\ UNCHANGED pending
       ELSE UNCHANGED <<st, pending, unproved>>
    /\ UNCHANGED <<rec, made, manifest, seen, ep, base, ghost>>

\* The output gate: acknowledge only if rec still says we own this epoch.
Prove(p) ==
    /\ st[p] = "serving"
    /\ unproved[p] # {}
    /\ IF rec = Mine(p)
       THEN /\ ackedIn' = [w \in Writes |-> IF w \in unproved[p] THEN ep[p] ELSE ackedIn[w]]
            /\ proven' = [proven EXCEPT ![ep[p]] = base[p] \o Flatten(segs[ep[p]])]
            /\ unproved' = [unproved EXCEPT ![p] = {}]
            /\ UNCHANGED <<st, pending>>
       ELSE /\ Reset(p)
            /\ UNCHANGED <<ackedIn, proven>>
    /\ UNCHANGED <<bucket, seen, ep, base, issued>>

Crash(p) ==
    /\ st[p] # "idle"
    /\ Reset(p)
    /\ UNCHANGED <<bucket, seen, ep, base, ghost>>

Next ==
    \/ \E p \in Procs : ReadRec(p) \/ Cas(p) \/ ReadBack(p) \/ Load(p) \/ WriteManifest(p)
                         \/ Upload(p) \/ Prove(p) \/ Crash(p)
    \/ \E p \in Procs, w \in Writes : Exec(p, w)
    \/ \E m \in inflight : Land(m)

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------
(* Invariants *)

TypeOK ==
    /\ rec.epoch \in 0..MaxEpoch
    /\ st \in [Procs -> {"idle", "read", "unsure", "loading", "manifest", "serving", "uploading"}]

\* At most one process serves any epoch.
OneWriterPerEpoch ==
    \A p, q \in Procs :
        (p # q /\ st[p] \in {"serving", "uploading"} /\ st[q] \in {"serving", "uploading"})
            => ep[p] # ep[q]

\* An acknowledged write is in the history of every later owner.
NoLostAckedWrite ==
    \A w \in Writes, e \in Epochs :
        (ackedIn[w] > 0 /\ e > ackedIn[w] /\ made[e]) => w \in Range(manifest[e])

\* What an owner proved to clients is where every later owner starts: no
\* write that was given up can reappear in the past.
NoHistoryRewrite ==
    \A e1, e2 \in Epochs : (e1 < e2 /\ made[e2]) => IsPrefix(proven[e1], manifest[e2])

=============================================================================
