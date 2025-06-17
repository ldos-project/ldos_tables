# LDOS Table implementations

This project contains test implementations of LDOS Tables for benchmarking and experimentation. This project will
eventually be subsumed into the LDOS kernel and deleted.

The current implementations are:

* A single-producer single-consumer table with optional support for strong and weak observers

**TODO**: 
* A redesign pass on the API.
* A multi-producer multi-consumer table with optional support for strong and weak observers
* Further optimizations. Search for `OPTIMIZATION:` in the code.

# Benchmark results

Qualitatively:

* Adding strong observer support costs little to nothing. This is because it only adds a load to check if a given strong
  observer slot is in use. The load is acquire ordering, so while it doesn't have to do an actual atomic operation, it
  might have a performance cost on relaxed memory models (ARM or RISC-V, but not x86).
* Cost per strong observer is roughly the cost of a consumer, so adding a single strong observer doubles the overall
  cost. This is very plausible given that single consumers and strong observers are implemented identically. ???
* Adding weak observer support costs 64bits per buffer element, and 2 atomic operations per send. This is significant,
  adding 10-20ns (~10%) to each send. However, this cost is only paid if weak observers are dynamically enabled at
  construction time, but it doesn't matter if one is actually registered.
* Weak observer accesses cause contention on the ring buffer, so they have a cost at read time. However, if the read
  frequency is relatively low the performance difference is very small.
* Transferring data via a table is very fast. Specifically, in a benchmark passing data via a 16-byte function parameter
  takes ~5ns and calling the function (with no parameters) and then reading the value from the table takes ~8ns. Because
  the "notification" of the new value in this case is a function call, there is no contention on the table. This is
  probably part of why the overhead is so low, but even with several weak observers the call is only ~10ns.


**TODO:**: 
* Proper benchmarking on Linux
* Benchmarking on 
