"""Control experiment for spike C.

Spike C measured that an 8-way PDFium worker pool only reached ~4.8x, and that
throughput stopped improving past ~16 workers. Before blaming PDFium or the
process-pool design, we need to know whether this machine scales at all under
sustained multi-process load. Mobile APUs are commonly power- and
thermal-limited, so a pure-compute workload may saturate at the same point.

If pure compute scales to ~16-32x, the saturation is specific to the PDF
workload and worth designing around. If it saturates at ~5x too, the ceiling is
the hardware, and the pool sizing decision is simply "match the machine".

Deliberately uses processes, not threads, so the comparison is like for like
with the worker-pool measurement.
"""

import multiprocessing
import time

DURATION = 2.0


def burn(seconds, queue):
    """Tight integer loop: no allocation, no I/O, no shared state."""
    end = time.perf_counter() + seconds
    count = 0
    state = 12345
    while time.perf_counter() < end:
        state = (state * 1103515245 + 12345) & 0x7FFFFFFF
        count += 1
    queue.put(count)


def main():
    print(f"{'procs':>6} {'wall s':>8} {'Miter/s':>10} {'vs 1 proc':>10}")
    baseline = None

    for procs in (1, 2, 4, 8, 16, 32):
        queue = multiprocessing.Queue()
        workers = [
            multiprocessing.Process(target=burn, args=(DURATION, queue))
            for _ in range(procs)
        ]

        start = time.perf_counter()
        for w in workers:
            w.start()
        for w in workers:
            w.join()
        wall = time.perf_counter() - start

        total = sum(queue.get() for _ in range(procs))
        rate = total / wall / 1e6

        if baseline is None:
            baseline = rate
        print(f"{procs:>6} {wall:>8.2f} {rate:>10.1f} {rate / baseline:>9.2f}x")


if __name__ == "__main__":
    main()
