/*
 * mprotect_futex_workload
 *
 * A small, repeatable workload for comparing the mprotect(2) and futex(2)
 * paths in Linux and CosmOS. Each worker owns an anonymous mapping and
 * repeatedly toggles one page between read-only and read-write, resembling a
 * JIT/runtime write window. The main thread and workers exchange a per-round
 * token through private futexes, so the futex calls include both waits and
 * wakeups rather than only failed (EAGAIN) probes.
 *
 * The binary is intentionally usable with both the native Linux compiler and
 * the static RISC-V musl compiler used by the CosmOS QEMU runner.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <inttypes.h>
#include <limits.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#ifndef SYS_futex
#if defined(__riscv) || defined(__aarch64__)
#define SYS_futex 98
#elif defined(__x86_64__)
#define SYS_futex 202
#elif defined(__i386__)
#define SYS_futex 240
#else
#error "SYS_futex is not available for this architecture"
#endif
#endif

#ifndef FUTEX_WAIT
#define FUTEX_WAIT 0
#endif

#ifndef FUTEX_WAKE
#define FUTEX_WAKE 1
#endif

#ifndef FUTEX_PRIVATE_FLAG
#define FUTEX_PRIVATE_FLAG 128
#endif

#define DEFAULT_WORKERS 4U
#define DEFAULT_ROUNDS 2048U
#define DEFAULT_PAGES 32U
#define MAX_WORKERS 128U
#define MAX_ROUNDS 100000000U
#define MAX_PAGES 65536U

struct counters {
    _Atomic unsigned long long mprotect_calls;
    _Atomic unsigned long long mprotect_failures;
    _Atomic unsigned long long futex_wait_calls;
    _Atomic unsigned long long futex_wake_calls;
    _Atomic unsigned long long futex_wait_eagain;
    _Atomic unsigned long long futex_wait_eintr;
    _Atomic unsigned long long futex_wake_returns;
    _Atomic unsigned long long futex_failures;
    _Atomic unsigned long long checksum;
    _Atomic int fatal;
};

struct worker {
    unsigned id;
    unsigned rounds;
    size_t page_size;
    size_t page_count;
    size_t mapping_len;
    unsigned char *mapping;
    _Atomic int parked;
    _Atomic int turn;
    _Atomic int done;
};

static struct counters g_counters;

static uint64_t monotonic_ns(void) {
    struct timespec ts;

    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        return 0;
    }
    return (uint64_t)ts.tv_sec * UINT64_C(1000000000) + (uint64_t)ts.tv_nsec;
}

static long raw_futex(_Atomic int *address, int operation, int value) {
    return syscall(SYS_futex, (int *)address, operation, value, NULL, NULL, 0);
}

static int futex_wake_one(_Atomic int *address) {
    long result;

    atomic_fetch_add_explicit(&g_counters.futex_wake_calls, 1,
                              memory_order_relaxed);
    result = raw_futex(address, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 1);
    if (result < 0) {
        atomic_fetch_add_explicit(&g_counters.futex_failures, 1,
                                  memory_order_relaxed);
        atomic_store_explicit(&g_counters.fatal, 1, memory_order_release);
        return -1;
    }
    atomic_fetch_add_explicit(&g_counters.futex_wake_returns,
                              (unsigned long long)result,
                              memory_order_relaxed);
    return 0;
}

/*
 * Wait until address reaches target. The value is re-read after every return,
 * which is the required futex protocol for both spurious wakeups and the
 * EAGAIN race between the user load and the kernel enqueue.
 */
static int futex_wait_until(_Atomic int *address, int target) {
    for (;;) {
        int observed;
        long result;
        int saved_errno;

        if (atomic_load_explicit(&g_counters.fatal, memory_order_acquire) != 0) {
            return -1;
        }

        observed = atomic_load_explicit(address, memory_order_acquire);
        if (observed >= target) {
            return 0;
        }

        atomic_fetch_add_explicit(&g_counters.futex_wait_calls, 1,
                                  memory_order_relaxed);
        result = raw_futex(address, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, observed);
        if (result == 0) {
            continue;
        }

        saved_errno = errno;
        if (saved_errno == EAGAIN) {
            atomic_fetch_add_explicit(&g_counters.futex_wait_eagain, 1,
                                      memory_order_relaxed);
            continue;
        }
        if (saved_errno == EINTR) {
            atomic_fetch_add_explicit(&g_counters.futex_wait_eintr, 1,
                                      memory_order_relaxed);
            continue;
        }

        atomic_fetch_add_explicit(&g_counters.futex_failures, 1,
                                  memory_order_relaxed);
        atomic_store_explicit(&g_counters.fatal, 1, memory_order_release);
        return -1;
    }
}

static int protect_page(void *address, size_t page_size, int protection) {
    int result;

    atomic_fetch_add_explicit(&g_counters.mprotect_calls, 1,
                              memory_order_relaxed);
    result = mprotect(address, page_size, protection);
    if (result != 0) {
        atomic_fetch_add_explicit(&g_counters.mprotect_failures, 1,
                                  memory_order_relaxed);
    }
    return result;
}

static void *worker_main(void *opaque) {
    struct worker *worker = (struct worker *)opaque;

    for (unsigned round = 1; round <= worker->rounds; ++round) {
        size_t page_index;
        unsigned char *page;
        unsigned char value;

        atomic_store_explicit(&worker->parked, (int)round,
                              memory_order_release);
        if (futex_wake_one(&worker->parked) != 0) {
            break;
        }
        if (futex_wait_until(&worker->turn, (int)round) != 0) {
            atomic_store_explicit(&worker->done, (int)round,
                                  memory_order_release);
            (void)futex_wake_one(&worker->done);
            break;
        }

        page_index = ((size_t)round * 13U + (size_t)worker->id * 7U) %
                     worker->page_count;
        page = worker->mapping + page_index * worker->page_size;

        /* A write window like a runtime patching/JIT cycle. */
        if (protect_page(page, worker->page_size, PROT_READ) != 0) {
            atomic_store_explicit(&g_counters.fatal, 1, memory_order_release);
            atomic_store_explicit(&worker->done, (int)round,
                                  memory_order_release);
            (void)futex_wake_one(&worker->done);
            break;
        }
        value = *(volatile unsigned char *)page;
        if (protect_page(page, worker->page_size, PROT_READ | PROT_WRITE) != 0) {
            atomic_store_explicit(&g_counters.fatal, 1, memory_order_release);
            atomic_store_explicit(&worker->done, (int)round,
                                  memory_order_release);
            (void)futex_wake_one(&worker->done);
            break;
        }
        *(volatile unsigned char *)page =
            (unsigned char)(value + (unsigned char)round + (unsigned char)worker->id);
        atomic_fetch_add_explicit(&g_counters.checksum, value,
                                  memory_order_relaxed);

        atomic_store_explicit(&worker->done, (int)round,
                              memory_order_release);
        if (futex_wake_one(&worker->done) != 0) {
            break;
        }
    }

    return NULL;
}

static int parse_positive(const char *text, unsigned maximum, unsigned *value) {
    char *end = NULL;
    unsigned long parsed;

    errno = 0;
    parsed = strtoul(text, &end, 10);
    if (errno != 0 || end == text || *end != '\0' || parsed == 0 ||
        parsed > maximum) {
        return -1;
    }
    *value = (unsigned)parsed;
    return 0;
}

static void print_usage(const char *program) {
    printf("Usage: %s [--workers N] [--rounds N] [--pages N]\n", program);
    printf("Defaults: workers=%u rounds=%u pages=%u\n", DEFAULT_WORKERS,
           DEFAULT_ROUNDS, DEFAULT_PAGES);
}

static int parse_options(int argc, char **argv, unsigned *workers,
                         unsigned *rounds, unsigned *pages) {
    for (int index = 1; index < argc; ++index) {
        const char *option = argv[index];
        unsigned *destination = NULL;
        unsigned maximum = 0;

        if (strcmp(option, "--help") == 0 || strcmp(option, "-h") == 0) {
            print_usage(argv[0]);
            return 1;
        }
        if (index + 1 >= argc) {
            fprintf(stderr, "missing value for %s\n", option);
            return -1;
        }
        if (strcmp(option, "--workers") == 0) {
            destination = workers;
            maximum = MAX_WORKERS;
        } else if (strcmp(option, "--rounds") == 0) {
            destination = rounds;
            maximum = MAX_ROUNDS;
        } else if (strcmp(option, "--pages") == 0) {
            destination = pages;
            maximum = MAX_PAGES;
        } else {
            fprintf(stderr, "unknown option: %s\n", option);
            return -1;
        }
        if (parse_positive(argv[++index], maximum, destination) != 0) {
            fprintf(stderr, "invalid value for %s: %s\n", option, argv[index]);
            return -1;
        }
    }
    return 0;
}

static unsigned long long load_counter(_Atomic unsigned long long *counter) {
    return atomic_load_explicit(counter, memory_order_relaxed);
}

int main(int argc, char **argv) {
    unsigned workers_count = DEFAULT_WORKERS;
    unsigned rounds = DEFAULT_ROUNDS;
    unsigned pages = DEFAULT_PAGES;
    size_t page_size;
    size_t mapping_len;
    struct worker *workers = NULL;
    pthread_t *threads = NULL;
    unsigned created = 0;
    uint64_t start_ns;
    uint64_t end_ns;
    int option_result;
    int status = 0;

    option_result = parse_options(argc, argv, &workers_count, &rounds, &pages);
    if (option_result > 0) {
        return 0;
    }
    if (option_result < 0) {
        print_usage(argv[0]);
        return 2;
    }

    page_size = (size_t)sysconf(_SC_PAGESIZE);
    if (page_size == 0 || (page_size & (page_size - 1)) != 0) {
        fprintf(stderr, "invalid page size: %zu\n", page_size);
        return 2;
    }
    if ((size_t)pages > SIZE_MAX / page_size) {
        fprintf(stderr, "mapping size overflow\n");
        return 2;
    }
    mapping_len = (size_t)pages * page_size;

    workers = calloc(workers_count, sizeof(*workers));
    threads = calloc(workers_count, sizeof(*threads));
    if (workers == NULL || threads == NULL) {
        fprintf(stderr, "allocation failed for worker state\n");
        free(workers);
        free(threads);
        return 2;
    }

    for (unsigned index = 0; index < workers_count; ++index) {
        void *mapping = mmap(NULL, mapping_len, PROT_READ | PROT_WRITE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (mapping == MAP_FAILED) {
            fprintf(stderr, "mmap worker %u failed: %s\n", index,
                    strerror(errno));
            status = 1;
            goto cleanup_mappings;
        }

        workers[index].id = index;
        workers[index].rounds = rounds;
        workers[index].page_size = page_size;
        workers[index].page_count = pages;
        workers[index].mapping_len = mapping_len;
        workers[index].mapping = (unsigned char *)mapping;
        atomic_init(&workers[index].parked, 0);
        atomic_init(&workers[index].turn, 0);
        atomic_init(&workers[index].done, 0);

        /* Prefault one byte per page before permission changes begin. */
        for (unsigned page = 0; page < pages; ++page) {
            workers[index].mapping[(size_t)page * page_size] =
                (unsigned char)(index + page + 1U);
        }
    }

    atomic_init(&g_counters.mprotect_calls, 0);
    atomic_init(&g_counters.mprotect_failures, 0);
    atomic_init(&g_counters.futex_wait_calls, 0);
    atomic_init(&g_counters.futex_wake_calls, 0);
    atomic_init(&g_counters.futex_wait_eagain, 0);
    atomic_init(&g_counters.futex_wait_eintr, 0);
    atomic_init(&g_counters.futex_wake_returns, 0);
    atomic_init(&g_counters.futex_failures, 0);
    atomic_init(&g_counters.checksum, 0);
    atomic_init(&g_counters.fatal, 0);

    start_ns = monotonic_ns();
    for (unsigned index = 0; index < workers_count; ++index) {
        int result = pthread_create(&threads[index], NULL, worker_main,
                                    &workers[index]);
        if (result != 0) {
            fprintf(stderr, "pthread_create worker %u failed: %s\n", index,
                    strerror(result));
            status = 1;
            atomic_store_explicit(&g_counters.fatal, 1, memory_order_release);
            break;
        }
        ++created;
    }

    if (created != workers_count) {
        /* This path is only for setup failures; normal runs create all workers. */
        for (unsigned index = 0; index < created; ++index) {
            atomic_store_explicit(&workers[index].turn, rounds,
                                  memory_order_release);
            (void)futex_wake_one(&workers[index].turn);
        }
    } else {
        for (unsigned round = 1; round <= rounds; ++round) {
            for (unsigned index = 0; index < workers_count; ++index) {
                if (futex_wait_until(&workers[index].parked, (int)round) != 0) {
                    status = 1;
                    break;
                }
            }
            if (status != 0 ||
                atomic_load_explicit(&g_counters.fatal, memory_order_acquire) != 0) {
                status = 1;
                break;
            }

            /* Let parked workers reach FUTEX_WAIT before publishing the token. */
            (void)sched_yield();
            for (unsigned index = 0; index < workers_count; ++index) {
                atomic_store_explicit(&workers[index].turn, (int)round,
                                      memory_order_release);
                if (futex_wake_one(&workers[index].turn) != 0) {
                    status = 1;
                    break;
                }
            }
            if (status != 0) {
                break;
            }

            for (unsigned index = 0; index < workers_count; ++index) {
                if (futex_wait_until(&workers[index].done, (int)round) != 0) {
                    status = 1;
                    break;
                }
            }
            if (status != 0) {
                break;
            }
        }
    }

    for (unsigned index = 0; index < created; ++index) {
        int result = pthread_join(threads[index], NULL);
        if (result != 0) {
            fprintf(stderr, "pthread_join worker %u failed: %s\n", index,
                    strerror(result));
            status = 1;
        }
    }
    end_ns = monotonic_ns();

    {
        unsigned long long expected_mprotect =
            (unsigned long long)workers_count * rounds * 2ULL;
        unsigned long long actual_mprotect =
            load_counter(&g_counters.mprotect_calls);
        unsigned long long mprotect_failures =
            load_counter(&g_counters.mprotect_failures);
        unsigned long long futex_failures =
            load_counter(&g_counters.futex_failures);
        unsigned long long elapsed_ns = end_ns >= start_ns ? end_ns - start_ns : 0;

        if (actual_mprotect != expected_mprotect || mprotect_failures != 0 ||
            futex_failures != 0 ||
            atomic_load_explicit(&g_counters.fatal, memory_order_acquire) != 0) {
            status = 1;
        }

        printf("MPF_RESULT version=1 workers=%u rounds=%u pages=%u "
               "mprotect_calls=%llu expected_mprotect=%llu "
               "mprotect_failures=%llu futex_wait_calls=%llu "
               "futex_wake_calls=%llu futex_wait_eagain=%llu "
               "futex_wait_eintr=%llu futex_wake_returns=%llu "
               "futex_failures=%llu checksum=%llu elapsed_ns=%llu status=%d\n",
               workers_count, rounds, pages, actual_mprotect,
               expected_mprotect, mprotect_failures,
               load_counter(&g_counters.futex_wait_calls),
               load_counter(&g_counters.futex_wake_calls),
               load_counter(&g_counters.futex_wait_eagain),
               load_counter(&g_counters.futex_wait_eintr),
               load_counter(&g_counters.futex_wake_returns), futex_failures,
               load_counter(&g_counters.checksum), elapsed_ns, status);
    }

cleanup_mappings:
    for (unsigned index = 0; index < workers_count; ++index) {
        if (workers[index].mapping != NULL) {
            (void)munmap(workers[index].mapping, workers[index].mapping_len);
        }
    }
    free(threads);
    free(workers);
    return status;
}
