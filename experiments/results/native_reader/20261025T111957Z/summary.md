# Native reader and ordered regions

All exact SUM/count checks passed on 15 sampled corpus columns and 36 controlled region timing cells.

Container descriptors add 10.44% to pooled existing page bytes. Median reader initialization is 0.155 ms per sampled column, including checksum, description parsing, dictionary-order verification, and fact reconstruction. Initialization is measured once per column and is not a repeated benchmark.

| Pages | Selectivity | Candidate pages | Speedup over original Facts | Facts / same regional shortcuts |
|---:|---:|---:|---:|---:|
| 64 | 0.1% | 8 | 2.15x | 1.03x |
| 64 | 1.0% | 8 | 2.05x | 1.03x |
| 64 | 10.0% | 8 | 1.75x | 1.02x |
| 64 | 50.0% | 32 | 1.18x | 1.12x |
| 512 | 0.1% | 8 | 10.96x | 1.03x |
| 512 | 1.0% | 8 | 8.63x | 1.02x |
| 512 | 10.0% | 56 | 2.57x | 1.09x |
| 512 | 50.0% | 256 | 1.24x | 1.12x |
| 4,096 | 0.1% | 8 | 47.19x | 1.02x |
| 4,096 | 1.0% | 48 | 15.33x | 1.09x |
| 4,096 | 10.0% | 416 | 2.95x | 1.12x |
| 4,096 | 50.0% | 2048 | 1.35x | 1.26x |

The region workload contains four appended sorted batches with overlapping value ranges; it is synthetic. Ordinary Facts plans each page after global order breaks. Both regional planners use the same certificates and executor. Warm timings exclude native initialization and region construction. Candidate counts describe pages admitted for planning, not physical reads. Full queries still validate page alignment.

The reader loads/checksums the whole file into memory. No cold-I/O or general-corpus region-speedup claim follows. Metadata storage and initialization are charged explicitly. No canonical measurements or manuscript claims are replaced.
