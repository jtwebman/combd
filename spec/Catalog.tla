----------------------------- MODULE Catalog ------------------------------
(***************************************************************************)
(* combd's catalog: deploys, vars, secrets, the state of every app. The    *)
(* bucket keeps it as an append-only log, and every node keeps a copy.     *)
(*                                                                         *)
(* The bucket holds:                                                       *)
(*   log          the changes, in order. Slot n+1 is written only if it's  *)
(*                empty (an S3 conditional write: If-None-Match), so    *)
(*                two writers can never both take it: one order, no leader *)
(*                needed for safety.                                       *)
(*                                                                         *)
(* A write (a deploy, say): read the log's head n, check the change        *)
(* against the state at n (here: no two apps may hold one domain), write   *)
(* slot n+1, and acknowledge only once that write succeeded. If the slot   *)
(* was taken, someone wrote first: read again, check again.                *)
(*                                                                         *)
(* Every node keeps a copy (SQLite, in memory or on disk). Writers push    *)
(* each change to every node, and pushes may be lost, repeated, or come in *)
(* any order. A node applies entry n+1 only after n; a gap is fetched from *)
(* the bucket. A node may crash and keep its copy (on disk) or lose it (in *)
(* memory); either way it catches up from the bucket.                      *)
(*                                                                         *)
(* Leases, heartbeats, and leaders aren't modeled: they're for liveness    *)
(* (who pushes, who notices a node is down). Safety must hold without them.*)
(*                                                                         *)
(* The BUG_ constants switch on known-bad versions. Each must break an     *)
(* invariant, which proves the checker can see that kind of bug.           *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets

CONSTANTS Nodes, Apps, Domains, MaxLog,
          BUG_ACK_BEFORE_WRITE,  \* acknowledge, then write (it may never land)
          BUG_BLIND_SLOT,        \* write slot n+1 even if it's taken (no If-None-Match)
          BUG_APPLY_ANY_ORDER,   \* apply a push whatever its place in the log
          BUG_UNCOVERED_CHECK    \* check against this node's copy, but write at the
                                 \* log's head: the conditional write doesn't cover
                                 \* what was checked (per-app pointers, the first design)

Changes == [app : Apps, dom : Domains]

VARIABLES
    log,                \* the bucket
    phase, base, want,  \* each writer: its step, the head it read, its change
    copy, pushes,       \* each node's copy, and pushes on the way
    acked               \* history, for the invariants: changes acknowledged

vars == <<log, phase, base, want, copy, pushes, acked>>

Range(s) == {s[i] : i \in DOMAIN s}
IsPrefix(a, b) == Len(a) <= Len(b) /\ SubSeq(b, 1, Len(a)) = a

\* The state after a log: each app's latest change says its domain.
Latest(s, a) == LET is == {i \in DOMAIN s : s[i].app = a}
                IN IF is = {} THEN "none" ELSE s[CHOOSE i \in is : \A j \in is : j <= i].dom
Holder(s, d) == {a \in Apps : Latest(s, a) = d}

\* May change c go after log s? Its domain is free, or already its app's.
Valid(s, c) == Holder(s, c.dom) \subseteq {c.app}

Init ==
    /\ log = <<>>
    /\ phase = [n \in Nodes |-> "idle"]
    /\ base = [n \in Nodes |-> 0]
    /\ want = [n \in Nodes |-> [app |-> CHOOSE a \in Apps : TRUE, dom |-> CHOOSE d \in Domains : TRUE]]
    /\ copy = [n \in Nodes |-> <<>>]
    /\ pushes = {}
    /\ acked = {}

\* A writer reads the head and checks its change against the state there.
Prepare(n, c) ==
    /\ phase[n] = "idle"
    /\ Len(log) < MaxLog
    /\ IF BUG_UNCOVERED_CHECK
       THEN /\ Valid(copy[n], c)
            /\ base' = [base EXCEPT ![n] = -1]   \* "wherever the head is when we write"
       ELSE /\ Valid(log, c)
            /\ base' = [base EXCEPT ![n] = Len(log)]
    /\ want' = [want EXCEPT ![n] = c]
    /\ phase' = [phase EXCEPT ![n] = "write"]
    /\ acked' = IF BUG_ACK_BEFORE_WRITE THEN acked \cup {c} ELSE acked
    /\ UNCHANGED <<log, copy, pushes>>

\* Write slot base+1, only if it's empty. Then acknowledge, and push it.
Write(n) ==
    /\ phase[n] = "write"
    /\ Len(log) < MaxLog
    /\ base' = IF base[n] = -1 THEN [base EXCEPT ![n] = Len(log)] ELSE base
    /\ IF Len(log) = base'[n]
       THEN /\ log' = Append(log, want[n])
            /\ acked' = acked \cup {want[n]}
            /\ pushes' = pushes \cup {[to |-> m, at |-> base'[n] + 1, c |-> want[n]] : m \in Nodes}
       ELSE IF BUG_BLIND_SLOT /\ Len(log) >= base'[n] + 1
            THEN /\ log' = [log EXCEPT ![base'[n] + 1] = want[n]]
                 /\ acked' = acked \cup {want[n]}
                 /\ pushes' = pushes \cup {[to |-> m, at |-> base'[n] + 1, c |-> want[n]] : m \in Nodes}
            \* Taken: someone wrote first. Start over: read, check again.
            ELSE UNCHANGED <<log, acked, pushes>>
    /\ phase' = [phase EXCEPT ![n] = "idle"]
    /\ UNCHANGED <<want, copy>>

\* A writer that crashes before its write (its change was never acknowledged).
GiveUp(n) ==
    /\ phase[n] = "write"
    /\ phase' = [phase EXCEPT ![n] = "idle"]
    /\ UNCHANGED <<log, base, want, copy, pushes, acked>>

\* A push arrives: applied if it's the next entry; a duplicate or a later one
\* waits for the bucket. It may be kept (to arrive again) or dropped.
Deliver(p) ==
    /\ p \in pushes
    /\ copy' = IF p.at = Len(copy[p.to]) + 1 \/ BUG_APPLY_ANY_ORDER
               THEN [copy EXCEPT ![p.to] = Append(@, p.c)]
               ELSE copy
    /\ \E keep \in BOOLEAN : pushes' = IF keep THEN pushes ELSE pushes \ {p}
    /\ Len(copy'[p.to]) <= MaxLog
    /\ UNCHANGED <<log, phase, base, want, acked>>

Lose(p) ==
    /\ p \in pushes
    /\ pushes' = pushes \ {p}
    /\ UNCHANGED <<log, phase, base, want, copy, acked>>

\* Catch up from the bucket: the next entry this node doesn't have.
Fetch(n) ==
    /\ Len(copy[n]) < Len(log)
    /\ copy' = [copy EXCEPT ![n] = Append(@, log[Len(copy[n]) + 1])]
    /\ UNCHANGED <<log, phase, base, want, pushes, acked>>

\* A node crashes: its copy survives on disk, or (in memory) is gone.
Crash(n) ==
    /\ \E keep \in BOOLEAN : copy' = IF keep THEN copy ELSE [copy EXCEPT ![n] = <<>>]
    /\ phase' = [phase EXCEPT ![n] = "idle"]
    /\ UNCHANGED <<log, base, want, pushes, acked>>

Next ==
    \/ \E n \in Nodes, c \in Changes : Prepare(n, c)
    \/ \E n \in Nodes : Write(n) \/ GiveUp(n) \/ Fetch(n) \/ Crash(n)
    \/ \E p \in pushes : Deliver(p) \/ Lose(p)

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------
(* Invariants *)

TypeOK ==
    /\ Len(log) <= MaxLog
    /\ phase \in [Nodes -> {"idle", "write"}]

\* An acknowledged change is in the bucket's log.
NoLostAckedChange == \A c \in acked : c \in Range(log)

\* Every node's copy is the start of the log: never a change the log
\* doesn't have, never one out of order.
CopyIsPrefix == \A n \in Nodes : IsPrefix(copy[n], log)

\* No two apps ever hold one domain: in the log, or in any node's copy.
OneAppPerDomain ==
    \A d \in Domains : /\ Cardinality(Holder(log, d)) <= 1
                       /\ \A n \in Nodes : Cardinality(Holder(copy[n], d)) <= 1

=============================================================================
