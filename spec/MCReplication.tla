---- MODULE MCReplication ----
\* The D3 model TLC checks: two owner processes (nodes A and B), two peers.
EXTENDS Replication, TLC
MCProcs == {"p1", "p2"}
MCNodeOf == [x \in MCProcs |-> IF x = "p1" THEN "A" ELSE "B"]
CONSTANTS r1, r2, w1, w2  \* model values
MCPeers == {r1, r2}
MCWrites == {w1, w2}
\* The peers are interchangeable, and so are the writes: TLC checks one of
\* each set of mirror-image states. Sound for invariants (all we check).
MCSym == Permutations(MCPeers) \cup Permutations(MCWrites)
====
