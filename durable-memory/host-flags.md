# Host flag cutover

Only --lmcode, --lmcode-schema and --lmcode-signatures select the host protocol.
Old modes are usage exit 2 before reading input. The established unreadable-input
policy remains application exit 4; malformed bytecode still uses load exit 3.
No .dslb paths, DSLB magic, cleanup selection or stdout protocol are changed.

Cargo now pins SDK f6b50c41905e925c8f662ec62e224226de8c1591 by immutable git
revision/lock instead of an unowned sibling checkout. The complete gate passes
12 unit tests and real temporary-tree dry-run/exec/trash/restore, exact sizes,
hidden/ignore behavior, stale directory matching, failure codes and JSON stdout.
The original CLI makes all three old-mode controls fail; the renamed CLI passes.
Both tracked-source and exact resolved SDK spelling scans pass.
