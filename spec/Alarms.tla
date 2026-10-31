---------------------------- MODULE Alarms ----------------------------
(***************************************************************************)
(* combd's alarm index in a fleet. An object's alarm lives in its SQLite, *)
(* so it travels with the object's history. The bucket keeps an index of   *)
(* alarms (keys alarm/<obj>/<due>-<id>), so a node can find a due alarm of *)
(* an object nobody is running. The claim: every state the object could be *)
(* restored to has its alarm in the index. Otherwise that alarm never      *)
(* fires, until something else wakes the object.                          *)
(*                                                                         *)
(* Replication is abstract here (Replication.tla checks it). `hist` is the *)
(* sealed history, one alarm per sealed state. Every state from the last   *)
(* acknowledged one (`acked`) to the last sealed one may be what the next  *)
(* owner restores. A claim restores one of them, and what came after it is *)
(* gone. That's more than the real protocol allows (once a manifest is     *)
(* written, the restored state is fixed), so a proof here holds there.    *)
(*                                                                         *)
(* An owner:                                                               *)
(*   - commits requests, each may set or clear the alarm (a transaction    *)
(*     is open in between, and its alarm isn't committed yet);             *)
(*   - before a seal goes up, has a key for the committed alarm that it    *)
(*     put itself and saw land (a key it only listed could be an older    *)
(*     owner's, which that owner may be deleting);                         *)
(*   - deletes a key only once a state is proven, and only if its due time *)
(*     is the alarm of no state from the proven one on (sealed or not), or *)
(*     it has its own landed key for that time already.                   *)
(* Each put is a new key. A bucket call that fails may still land later,   *)
(* so a key once deleted is never put again.                               *)
(*                                                                         *)
(* Old owners keep acting until they notice (they can't prove anything     *)
(* now, so they can't seal or delete more than before), crash, and their   *)
(* bucket calls land whenever they land.                                  *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets

CONSTANTS Times,       \* due times an alarm may have
          MaxEpoch,    \* owners, one after another
          MaxHist,     \* sealed states
          MaxCommits,  \* commits, all owners
          MaxPuts,     \* index keys put, all owners
          BUG_DELETE_ON_COMMIT,   \* delete a key once the committed alarm moves (as before)
          BUG_SEAL_READS_TXN,     \* a seal reads the alarm mid-transaction (as before)
          BUG_SEAL_TRUSTS_LISTED, \* a seal counts a key it listed, not one it put
          BUG_REUSE_KEYS,         \* a key is its due time alone (as before)
          BUG_RESTORE_KEEPS_NEWEST \* a restore deletes all but the latest key at once (as before)

Nil == 0
Unknown == -1
NoTxn == -2
Epochs == 1..MaxEpoch
Alarms == Times \cup {Nil}

VARIABLES hist, acked, cur, ncommit, nextId,
          idx,   \* the index in the bucket: <<due, id>>
          ops,   \* bucket calls out: may land, and may report, in any order
          on, base, val, txn, prov, pend, known, mine,
          listed \* what the owner's restore listed

vars == <<hist, acked, cur, ncommit, nextId, idx, ops, on, base, val, txn, prov, pend, known, mine, listed>>
owner == <<on, base, val, txn, prov, pend, known, mine, listed>>

\* An owner's states from its proven one on: the alarms it must keep.
\* pend: [a |-> alarm, s |-> the seal that covers it, 0 if none yet].
Live(e) == {prov[e]} \cup {pend[e][j].a : j \in 1..Len(pend[e])}

Init ==
    /\ hist = <<Nil>>
    /\ acked = 1
    /\ cur = 0
    /\ ncommit = 0
    /\ nextId = 0
    /\ idx = {}
    /\ ops = {}
    /\ on = [e \in Epochs |-> FALSE]
    /\ base = [e \in Epochs |-> 0]
    /\ val = [e \in Epochs |-> Nil]
    /\ txn = [e \in Epochs |-> NoTxn]
    /\ prov = [e \in Epochs |-> Unknown]
    /\ pend = [e \in Epochs |-> <<>>]
    /\ known = [e \in Epochs |-> {}]
    /\ mine = [e \in Epochs |-> {}]
    /\ listed = [e \in Epochs |-> {}]

\* A new owner restores any state that could be restored, and lists the index.
Claim(i) ==
    LET e == cur + 1 IN
    /\ cur < MaxEpoch
    /\ i \in acked..Len(hist)
    /\ hist' = SubSeq(hist, 1, i)
    /\ cur' = e
    /\ on' = [on EXCEPT ![e] = TRUE]
    /\ base' = [base EXCEPT ![e] = i]
    /\ val' = [val EXCEPT ![e] = hist[i]]
    /\ pend' = [pend EXCEPT ![e] = <<[a |-> hist[i], s |-> i]>>]
    /\ known' = [known EXCEPT ![e] = idx]
    /\ listed' = [listed EXCEPT ![e] = idx]
    /\ UNCHANGED <<acked, ncommit, nextId, idx, ops, txn, prov, mine>>

Crash(e) ==
    /\ on[e]
    /\ on' = [on EXCEPT ![e] = FALSE]
    /\ UNCHANGED <<hist, acked, cur, ncommit, nextId, idx, ops, base, val, txn, prov, pend, known, mine, listed>>

\* A request sets or clears the alarm, and commits. Its transaction is
\* open in between, but nothing in the protocol reads an open transaction,
\* so it's modeled only where a known-bad version does (BUG_SEAL_READS_TXN):
\* Begin, then Commit or Abort. Elsewhere a commit is one step.
Begin(e, a) ==
    /\ BUG_SEAL_READS_TXN
    /\ on[e] /\ txn[e] = NoTxn /\ ncommit < MaxCommits
    /\ txn' = [txn EXCEPT ![e] = a]
    /\ UNCHANGED <<hist, acked, cur, ncommit, nextId, idx, ops, on, base, val, prov, pend, known, mine, listed>>

Committed(e, a) ==
    /\ val' = [val EXCEPT ![e] = a]
    /\ pend' = [pend EXCEPT ![e] = Append(@, [a |-> a, s |-> 0])]
    /\ ncommit' = ncommit + 1

Commit(e) ==
    /\ on[e] /\ txn[e] # NoTxn
    /\ Committed(e, txn[e])
    /\ txn' = [txn EXCEPT ![e] = NoTxn]
    /\ UNCHANGED <<hist, acked, cur, nextId, idx, ops, on, base, prov, known, mine, listed>>

CommitNow(e, a) ==
    /\ ~BUG_SEAL_READS_TXN
    /\ on[e] /\ ncommit < MaxCommits
    /\ Committed(e, a)
    /\ UNCHANGED <<hist, acked, cur, nextId, idx, ops, on, base, txn, prov, known, mine, listed>>

Abort(e) ==
    /\ on[e] /\ txn[e] # NoTxn
    /\ txn' = [txn EXCEPT ![e] = NoTxn]
    /\ UNCHANGED <<hist, acked, cur, ncommit, nextId, idx, ops, on, base, val, prov, pend, known, mine, listed>>

\* What's committed since the last seal goes up as the next state, once
\* the index has its alarm. Only the current owner's seals can be restored.
Seal(e) ==
    LET a == IF BUG_SEAL_READS_TXN /\ txn[e] # NoTxn THEN txn[e] ELSE val[e]
        trusted == IF BUG_SEAL_TRUSTS_LISTED THEN mine[e] \cup (known[e] \cap listed[e]) ELSE mine[e]
        p == Len(hist) + 1
    IN
    /\ on[e] /\ e = cur /\ Len(hist) < MaxHist
    /\ \E j \in 1..Len(pend[e]) : pend[e][j].s = 0
    /\ a = Nil \/ \E k \in trusted : k[1] = a
    /\ hist' = Append(hist, val[e])
    /\ pend' = [pend EXCEPT ![e] = [j \in 1..Len(@) |-> IF @[j].s = 0 THEN [@[j] EXCEPT !.s = p] ELSE @[j]]]
    /\ UNCHANGED <<acked, cur, ncommit, nextId, idx, ops, on, base, val, txn, prov, known, mine, listed>>

\* The current owner proves a state of its own (or the one it restored):
\* an answer goes out. States before it can't come back.
Ack(e, p) ==
    /\ on[e] /\ e = cur
    /\ p \in (IF acked > base[e] THEN acked ELSE base[e])..Len(hist)
    /\ acked' = p
    /\ prov' = [prov EXCEPT ![e] = hist[p]]
    /\ pend' = [pend EXCEPT ![e] = SelectSeq(@, LAMBDA r : r.s = 0 \/ r.s > p)]
    /\ UNCHANGED <<hist, cur, ncommit, nextId, idx, ops, on, base, val, txn, known, mine, listed>>

\* A call by e for key k, not yet answered.
Out(e, k) == \E o \in ops : o.e = e /\ o.k = k /\ ~o.rep

Put(e) ==
    LET t == val[e]
        k == <<t, IF BUG_REUSE_KEYS THEN 0 ELSE nextId + 1>>
    IN
    /\ on[e] /\ t # Nil /\ nextId < MaxPuts
    /\ ~\E m \in mine[e] : m[1] = t
    /\ ~\E o \in ops : o.e = e /\ o.put /\ o.k[1] = t /\ ~o.rep
    /\ ~Out(e, k)
    /\ nextId' = nextId + 1
    /\ known' = [known EXCEPT ![e] = @ \cup {k}]
    /\ ops' = ops \cup {[e |-> e, put |-> TRUE, k |-> k, land |-> FALSE, rep |-> FALSE]}
    /\ UNCHANGED <<hist, acked, cur, ncommit, idx, on, base, val, txn, prov, pend, mine, listed>>

Deletable(e, k) ==
    LET t == k[1] IN
    IF BUG_DELETE_ON_COMMIT THEN t # val[e]
    ELSE IF BUG_RESTORE_KEEPS_NEWEST /\ prov[e] = Unknown
         THEN \E m \in known[e] : m[1] > t
    ELSE /\ prov[e] # Unknown
         /\ \/ t \notin Live(e)
            \/ \E m \in mine[e] : m[1] = t /\ m # k

Delete(e, k) ==
    /\ on[e] /\ k \in known[e] /\ ~Out(e, k)
    /\ Deletable(e, k)
    /\ mine' = [mine EXCEPT ![e] = @ \ {k}]
    /\ ops' = ops \cup {[e |-> e, put |-> FALSE, k |-> k, land |-> FALSE, rep |-> FALSE]}
    /\ UNCHANGED <<hist, acked, cur, ncommit, nextId, idx, on, base, val, txn, prov, pend, known, listed>>

\* Done with a call once it has landed and been answered.
Keep(o) == IF o.land /\ o.rep THEN {} ELSE {o}

Land(o) ==
    /\ o \in ops /\ ~o.land
    /\ idx' = IF o.put THEN idx \cup {o.k} ELSE idx \ {o.k}
    /\ ops' = (ops \ {o}) \cup Keep([o EXCEPT !.land = TRUE])
    /\ UNCHANGED <<hist, acked, cur, ncommit, nextId, owner>>

\* It worked: only once it has landed.
ReplyOk(o) ==
    /\ o \in ops /\ o.land /\ ~o.rep
    /\ ops' = ops \ {o}
    /\ IF on[o.e]
       THEN IF o.put
            THEN /\ mine' = [mine EXCEPT ![o.e] = @ \cup {o.k}]
                 /\ UNCHANGED known
            ELSE /\ known' = [known EXCEPT ![o.e] = @ \ {o.k}]
                 /\ UNCHANGED mine
       ELSE UNCHANGED <<mine, known>>
    /\ UNCHANGED <<hist, acked, cur, ncommit, nextId, idx, on, base, val, txn, prov, pend, listed>>

\* It failed, or timed out: it may still land, or never. The key stays
\* known (maybe there), so a later delete can clean it up.
ReplyFail(o) ==
    /\ o \in ops /\ ~o.rep
    /\ ops' = (ops \ {o}) \cup Keep([o EXCEPT !.rep = TRUE])
    /\ UNCHANGED <<hist, acked, cur, ncommit, nextId, idx, owner>>

Next ==
    \/ \E i \in 1..MaxHist : Claim(i)
    \/ \E e \in Epochs :
        \/ Crash(e) \/ Commit(e) \/ Abort(e) \/ Seal(e) \/ Put(e)
        \/ \E a \in Alarms : Begin(e, a) \/ CommitNow(e, a)
        \/ \E p \in 1..MaxHist : Ack(e, p)
        \/ \E k \in known[e] : Delete(e, k)
    \/ \E o \in ops : Land(o) \/ ReplyOk(o) \/ ReplyFail(o)

Spec == Init /\ [][Next]_vars

\* Every state that could still be restored has its alarm in the index.
AlarmFound ==
    \A p \in acked..Len(hist) :
        hist[p] # Nil => \E k \in idx : k[1] = hist[p]
=============================================================================
