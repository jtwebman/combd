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
(* The log doesn't grow forever: a node writes a snapshot (the state at   *)
(* slot k, from its copy, which is a prefix of the log), and only then are *)
(* slots up to k trimmed. A trimmed slot's key is gone, so it's empty      *)
(* again: a slow writer whose check was based on an old head could write  *)
(* it, and "only if empty" would let it. So after a write, the writer      *)
(* reads the snapshot: if it covers the slot, the write didn't count;      *)
(* check again and retry. But the stray bytes are in the slot now, and a   *)
(* reader catching up could read them: so a reader, too, reads the        *)
(* snapshot again after reading slots, and if it covers them, starts from  *)
(* the snapshot instead. (The simulator found the reader's half of this.)  *)
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
          BUG_TRIM_NO_GATE,      \* acknowledge a write without checking the snapshot
          BUG_TRIM_EARLY,        \* trim slots the snapshot doesn't cover yet
          BUG_READ_NO_GATE,      \* catch up without reading the snapshot again
          BUG_UNCOVERED_CHECK    \* check against this node's copy, but write at the
                                 \* log's head: the conditional write doesn't cover
                                 \* what was checked (per-app pointers, the first design)

Changes == [app : Apps, dom : Domains]

VARIABLES
    log,                \* the bucket: the log (all of it, for the invariants),
    snapAt, trimmed,    \* a snapshot of the state at slot snapAt, slots up to trimmed gone
    stray,              \* what a slow writer put in a trimmed slot: slot -> change
    phase, base, want,  \* each writer: its step, the head it read, its change
    copy, pushes,       \* each node's copy, and pushes on the way
    acked               \* history, for the invariants: changes acknowledged

vars == <<log, snapAt, trimmed, stray, phase, base, want, copy, pushes, acked>>

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
    /\ snapAt = 0
    /\ trimmed = 0
    /\ stray = [k \in {} |-> want]
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
    /\ UNCHANGED <<log, snapAt, trimmed, stray, copy, pushes>>

\* Write slot base+1, only if it's empty. Then acknowledge, and push it.
Write(n) ==
    /\ phase[n] = "write"
    /\ Len(log) < MaxLog
    /\ base' = IF base[n] = -1 THEN [base EXCEPT ![n] = Len(log)] ELSE base
    /\ IF Len(log) = base'[n]
       THEN /\ log' = Append(log, want[n])
            /\ acked' = acked \cup {want[n]}
            /\ pushes' = pushes \cup {[to |-> m, at |-> base'[n] + 1, c |-> want[n]] : m \in Nodes}
            /\ UNCHANGED stray
       \* A trimmed slot is empty again, so the write lands, where nobody will
       \* ever read it. The snapshot covers it: unless that check is skipped,
       \* the write doesn't count.
       ELSE IF base'[n] + 1 <= trimmed /\ base'[n] + 1 \notin DOMAIN stray
            THEN /\ acked' = IF BUG_TRIM_NO_GATE THEN acked \cup {want[n]} ELSE acked
                 /\ stray' = [k \in DOMAIN stray \cup {base'[n] + 1} |-> IF k = base'[n] + 1 THEN want[n] ELSE stray[k]]
                 /\ UNCHANGED <<log, pushes>>
       ELSE IF BUG_BLIND_SLOT /\ Len(log) >= base'[n] + 1
            THEN /\ log' = [log EXCEPT ![base'[n] + 1] = want[n]]
                 /\ acked' = acked \cup {want[n]}
                 /\ pushes' = pushes \cup {[to |-> m, at |-> base'[n] + 1, c |-> want[n]] : m \in Nodes}
                 /\ UNCHANGED stray
            \* Taken: someone wrote first. Start over: read, check again.
            ELSE UNCHANGED <<log, acked, pushes, stray>>
    /\ phase' = [phase EXCEPT ![n] = "idle"]
    /\ UNCHANGED <<snapAt, trimmed, want, copy>>

\* A writer that crashes before its write (its change was never acknowledged).
GiveUp(n) ==
    /\ phase[n] = "write"
    /\ phase' = [phase EXCEPT ![n] = "idle"]
    /\ UNCHANGED <<log, snapAt, trimmed, stray, base, want, copy, pushes, acked>>

\* A push arrives: applied if it's the next entry; a duplicate or a later one
\* waits for the bucket. It may be kept (to arrive again) or dropped.
Deliver(p) ==
    /\ p \in pushes
    /\ copy' = IF p.at = Len(copy[p.to]) + 1 \/ BUG_APPLY_ANY_ORDER
               THEN [copy EXCEPT ![p.to] = Append(@, p.c)]
               ELSE copy
    /\ \E keep \in BOOLEAN : pushes' = IF keep THEN pushes ELSE pushes \ {p}
    /\ Len(copy'[p.to]) <= MaxLog
    /\ UNCHANGED <<log, snapAt, trimmed, stray, phase, base, want, acked>>

Lose(p) ==
    /\ p \in pushes
    /\ pushes' = pushes \ {p}
    /\ UNCHANGED <<log, snapAt, trimmed, stray, phase, base, want, copy, acked>>

\* Catch up from the bucket: read the next slot. If it's trimmed, what's
\* there (if anything) is a stray write that never counted: reading the
\* snapshot again shows it's covered, so the node starts from the snapshot
\* (the state at snapAt). The bug skips that, and takes the stray bytes.
Fetch(n) ==
    /\ Len(copy[n]) < Len(log)
    /\ LET next == Len(copy[n]) + 1
       IN IF next > trimmed
          THEN copy' = [copy EXCEPT ![n] = Append(@, log[next])]
          ELSE IF BUG_READ_NO_GATE /\ next \in DOMAIN stray
               THEN copy' = [copy EXCEPT ![n] = Append(@, stray[next])]
               ELSE /\ snapAt > Len(copy[n])
                    /\ copy' = [copy EXCEPT ![n] = SubSeq(log, 1, snapAt)]
    /\ UNCHANGED <<log, snapAt, trimmed, stray, phase, base, want, pushes, acked>>

\* A node writes a snapshot of its copy (the state at its length).
Snapshot(n) ==
    /\ Len(copy[n]) > snapAt
    /\ snapAt' = Len(copy[n])
    /\ UNCHANGED <<log, trimmed, stray, phase, base, want, copy, pushes, acked>>

\* Trim: delete the slots the snapshot covers (or, the bug, more).
Trim ==
    /\ \E k \in (trimmed + 1)..(IF BUG_TRIM_EARLY THEN Len(log) ELSE snapAt) : trimmed' = k
    /\ UNCHANGED <<log, snapAt, stray, phase, base, want, copy, pushes, acked>>

\* A node crashes: its copy survives on disk, or (in memory) is gone.
Crash(n) ==
    /\ \E keep \in BOOLEAN : copy' = IF keep THEN copy ELSE [copy EXCEPT ![n] = <<>>]
    /\ phase' = [phase EXCEPT ![n] = "idle"]
    /\ UNCHANGED <<log, snapAt, trimmed, stray, base, want, pushes, acked>>

Next ==
    \/ \E n \in Nodes, c \in Changes : Prepare(n, c)
    \/ \E n \in Nodes : Write(n) \/ GiveUp(n) \/ Fetch(n) \/ Crash(n) \/ Snapshot(n)
    \/ Trim
    \/ \E p \in pushes : Deliver(p) \/ Lose(p)

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------
(* Invariants *)

TypeOK ==
    /\ Len(log) <= MaxLog
    /\ phase \in [Nodes -> {"idle", "write"}]

\* An acknowledged change is in the bucket's log.
NoLostAckedChange == \A c \in acked : c \in Range(log)

\* Every node can still catch up: what it's missing is in a slot not yet
\* trimmed, or in the snapshot. (Trimming early strands a node: its next
\* slot is gone, and the snapshot is older than that.)
CanCatchUp == \A n \in Nodes : Len(copy[n]) + 1 > trimmed \/ snapAt >= trimmed

\* Every node's copy is the start of the log: never a change the log
\* doesn't have, never one out of order.
CopyIsPrefix == \A n \in Nodes : IsPrefix(copy[n], log)

\* No two apps ever hold one domain: in the log, or in any node's copy.
OneAppPerDomain ==
    \A d \in Domains : /\ Cardinality(Holder(log, d)) <= 1
                       /\ \A n \in Nodes : Cardinality(Holder(copy[n], d)) <= 1

=============================================================================
