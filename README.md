# big

A bitmap-native analytical database.

A b-tree of roaring containers over 8 KB pages, pure copy-on-write, with the read path borrowing
straight out of the mapped file. Every fact is one bit at `(row, record)`, so a filter is an
intersection and a count is a population count rather than a scan.
