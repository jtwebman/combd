---- MODULE MCReplication ----
\* The D3 model TLC checks: two owner processes (nodes A and B), two peers.
EXTENDS Replication
MCProcs == {"p1", "p2"}
MCNodeOf == [x \in MCProcs |-> IF x = "p1" THEN "A" ELSE "B"]
MCPeers == {"r1", "r2"}
MCWrites == {"w1", "w2"}
====
