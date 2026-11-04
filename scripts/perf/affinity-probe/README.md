# Inherited AF_XDP CPU affinity

The production selector used `ordinal % num_cpus::get()` as a CPU ID. A CPU
count does not identify the CPUs allowed by a sparse taskset/cpuset. Queue zero
therefore selected CPU 0 even when the supervisor inherited a different mask.
The replacement enumerates the actual `sched_getaffinity` mask once at startup;
explicit `interfaces[].cpus` entries keep their precedence and are never remapped.

`run.py` extracts the exact old and new selectors plus the old pinning function,
and compiles them with the new production affinity module. It runs independent
processes with an inherited mask excluding CPU 0. The old placement assertion
must fail, while the new selector must choose and pin to an inherited CPU. Both
versions also check explicit mapping selection. The old fixture supplies the
restricted CPU count directly: queue zero selects zero for **any** positive
count, including a quota-adjusted `num_cpus::get()`. This fixture is not a model
of quota discovery or a full-node throughput benchmark.

The observation process retains whether old pinning fails or expands the
taskset mask (when the enclosing cgroup permits CPU 0). It only changes its own
process affinity. The parent mask must remain unchanged. Production unit tests
cover sparse IDs, a mask spanning words, invalid indices, and a separate worker
whose real affinity is read back after pinning.

On Linux kernels with a larger CPU mask than `cpu_set_t`, the production query
grows an aligned word buffer up to 1 MiB. An unreadable inherited mask retires
the unstarted bridge lease and enters the existing explicit dataplane failure
path; redirect is never enabled with no worker. Pinning errors retain the
existing warning and inherited placement behavior, with the OS error included.

Run with `python3 scripts/perf/affinity-probe/run.py` in the isolated Actions
runner. `.local/affinity-ci` retains exact sources, build/test logs, the executed
binary, hashes and all positive/negative controls. Local Rust builds remain
subject to the repository mbx and disk-space requirements.

This is a placement and correctness repair. The prior UDP matrix configured
queue CPUs explicitly, so this bug cannot explain that matrix's AF_XDP gap.
