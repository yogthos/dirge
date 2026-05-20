# Shared state for the multi-file `turn_timer` plugin.
#
# Demonstrates how dirge loads every `*.janet` file in a plugin
# directory into the *same* Janet env, in alphabetical order. This
# file defines vars; the next file (`01-hooks.janet`) reads them.
#
# Compare with the single-file `turn_timing.janet` — same idea, but
# split across files to show off the multi-file layout.

(var turn-start-ms 0)
(var total-elapsed-ms 0)
(var turn-count 0)
