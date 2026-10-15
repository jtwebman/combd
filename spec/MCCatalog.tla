---- MODULE MCCatalog ----
\* The model TLC checks: two nodes (each writes, and keeps a copy), two
\* apps, two domains, a log of up to three changes.
EXTENDS Catalog
MCNodes == {"n1", "n2"}
MCApps == {"a1", "a2"}
MCDomains == {"d1", "d2"}
====
