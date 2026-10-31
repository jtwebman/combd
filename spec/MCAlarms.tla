---- MODULE MCAlarms ----
\* The alarm index model TLC checks. Due times are interchangeable unless a
\* version compares them (BUG_RESTORE_KEEPS_NEWEST keeps the latest).
EXTENDS Alarms, TLC
CONSTANTS t1, t2  \* model values
MCTimes == {t1, t2}
MCSym == Permutations(MCTimes)
====
