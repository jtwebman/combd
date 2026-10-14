---- MODULE MCOwnership ----
\* The model TLC checks: three processes, where p1 and p2 are two processes
\* of node A (a frozen one and its restart) and p3 is node B.
EXTENDS Ownership
MCProcs == {"p1", "p2", "p3"}
MCNodeOf == [p \in MCProcs |-> IF p = "p3" THEN "B" ELSE "A"]
MCWrites == {"w1", "w2"}
====
