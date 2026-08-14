/*
 * statx_bench
 *
 * A small lmbench-style raw statx(2) latency test.  The same static RISC-V
 * binary is run in a Linux guest and in CosmOS, so libc's stat/statx wrapper
 * is not part of the measured path.  The benchmark reports both pathname and
 * AT_EMPTY_PATH cases, plus a relative-dirfd case that exercises CosmOS's
 * inode-based lookup fast path.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#ifndef SYS_statx
#if defined(__riscv)
#define SYS_statx 291
#elif defined(__x86_64__)
#define SYS_statx 332
#else
#error "SYS_statx is not available for this architecture"
#endif
#endif

#ifndef AT_FDCWD
#define AT_FDCWD (-100)
#endif

#ifndef AT_EMPTY_PATH
#define AT_EMPTY_PATH 0x1000
#endif

#ifndef STATX_BASIC_STATS
#define STATX_BASIC_STATS 0x000007ffU
#endif

#ifndef STATX_BTIME
#define STATX_BTIME 0x00000800U
#endif

#define DEFAULT_ITERATIONS 100000UL
#define DEFAULT_WARMUP 1000UL
#define DEFAULT_FILES 128UL
#define DEFAULT_PASSES 16UL
#define MAX_ITERATIONS 100000000UL
#define MAX_FILES 4096UL
#define MAX_PASSES 100000UL
#define OUTPUT_BYTES 256U
#define BENCH_DIR "/var/tmp/statx-bench-files"
#define FIXED_PATH "/var/tmp/statx-bench-file"
#define PATH_BUFFER 256U

struct bench_context {
    unsigned mask;
    unsigned long file_count;
    int file_fd;
    int dir_fd;
    char **paths;
    unsigned char output[OUTPUT_BYTES];
};

typedef long (*statx_call)(struct bench_context *, unsigned long);

static uint64_t monotonic_ns(void) {
    struct timespec ts;

    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        fprintf(stderr, "clock_gettime(CLOCK_MONOTONIC) failed: %s\n",
                strerror(errno));
        exit(2);
    }
    return (uint64_t)ts.tv_sec * UINT64_C(1000000000) +
           (uint64_t)ts.tv_nsec;
}

static long raw_statx(int dirfd, const char *path, int flags, unsigned mask,
                      void *output) {
    return syscall(SYS_statx, dirfd, path, flags, mask, output);
}

static long call_fixed_path(struct bench_context *context,
                            unsigned long iteration) {
    (void)iteration;
    return raw_statx(AT_FDCWD, FIXED_PATH, 0, context->mask,
                     context->output);
}

static long call_empty_path(struct bench_context *context,
                            unsigned long iteration) {
    (void)iteration;
    return raw_statx(context->file_fd, "", AT_EMPTY_PATH, context->mask,
                     context->output);
}

static long call_relative_dirfd(struct bench_context *context,
                                unsigned long iteration) {
    (void)iteration;
    return raw_statx(context->dir_fd, "f0000", 0, context->mask,
                     context->output);
}

static long call_rotating_path(struct bench_context *context,
                               unsigned long iteration) {
    const char *path = context->paths[iteration % context->file_count];

    return raw_statx(AT_FDCWD, path, 0, context->mask, context->output);
}

static unsigned long parse_count(const char *option, const char *text,
                                 unsigned long maximum) {
    char *end = NULL;
    unsigned long value;

    errno = 0;
    value = strtoul(text, &end, 10);
    if (errno != 0 || end == text || *end != '\0' || value == 0 ||
        value > maximum) {
        fprintf(stderr, "invalid %s: %s\n", option, text);
        exit(2);
    }
    return value;
}

static void print_usage(const char *program) {
    printf("Usage: %s [--iterations N] [--files N] [--passes N] "
           "[--mask basic|all]\n",
           program);
    printf("Defaults: iterations=%lu files=%lu passes=%lu mask=all\n",
           DEFAULT_ITERATIONS, DEFAULT_FILES, DEFAULT_PASSES);
}

static void parse_options(int argc, char **argv, unsigned long *iterations,
                          unsigned long *files, unsigned long *passes,
                          unsigned *mask) {
    *iterations = DEFAULT_ITERATIONS;
    *files = DEFAULT_FILES;
    *passes = DEFAULT_PASSES;
    *mask = STATX_BASIC_STATS | STATX_BTIME;

    for (int index = 1; index < argc; ++index) {
        const char *option = argv[index];

        if (strcmp(option, "--help") == 0 || strcmp(option, "-h") == 0) {
            print_usage(argv[0]);
            exit(0);
        }
        if (index + 1 >= argc) {
            fprintf(stderr, "missing value for %s\n", option);
            exit(2);
        }
        if (strcmp(option, "--iterations") == 0) {
            *iterations = parse_count(option, argv[++index], MAX_ITERATIONS);
        } else if (strcmp(option, "--files") == 0) {
            *files = parse_count(option, argv[++index], MAX_FILES);
        } else if (strcmp(option, "--passes") == 0) {
            *passes = parse_count(option, argv[++index], MAX_PASSES);
        } else if (strcmp(option, "--mask") == 0) {
            const char *value = argv[++index];
            if (strcmp(value, "basic") == 0) {
                *mask = STATX_BASIC_STATS;
            } else if (strcmp(value, "all") == 0) {
                *mask = STATX_BASIC_STATS | STATX_BTIME;
            } else {
                fprintf(stderr, "invalid --mask: %s\n", value);
                exit(2);
            }
        } else {
            fprintf(stderr, "unknown option: %s\n", option);
            print_usage(argv[0]);
            exit(2);
        }
    }
}

static void create_file(const char *path) {
    int fd = open(path, O_CREAT | O_RDWR, 0600);

    if (fd < 0) {
        fprintf(stderr, "open(%s) failed: %s\n", path, strerror(errno));
        exit(1);
    }
    if (close(fd) != 0) {
        fprintf(stderr, "close(%s) failed: %s\n", path, strerror(errno));
        exit(1);
    }
}

static void prepare_files(struct bench_context *context) {
    if (mkdir(BENCH_DIR, 0700) != 0 && errno != EEXIST) {
        fprintf(stderr, "mkdir(%s) failed: %s\n", BENCH_DIR, strerror(errno));
        exit(1);
    }
    create_file(FIXED_PATH);

    context->paths = calloc(context->file_count, sizeof(*context->paths));
    if (context->paths == NULL) {
        fprintf(stderr, "calloc paths failed\n");
        exit(1);
    }
    for (unsigned long index = 0; index < context->file_count; ++index) {
        int length;

        context->paths[index] = malloc(PATH_BUFFER);
        if (context->paths[index] == NULL) {
            fprintf(stderr, "malloc path failed at %lu\n", index);
            exit(1);
        }
        length = snprintf(context->paths[index], PATH_BUFFER, "%s/f%04lu",
                          BENCH_DIR, index);
        if (length < 0 || (unsigned)length >= PATH_BUFFER) {
            fprintf(stderr, "path formatting failed at %lu\n", index);
            exit(1);
        }
        create_file(context->paths[index]);
    }

    context->file_fd = open(FIXED_PATH, O_RDONLY);
    if (context->file_fd < 0) {
        fprintf(stderr, "open fixed file failed: %s\n", strerror(errno));
        exit(1);
    }
    context->dir_fd = open(BENCH_DIR, O_RDONLY);
    if (context->dir_fd < 0) {
        fprintf(stderr, "open benchmark directory failed: %s\n",
                strerror(errno));
        exit(1);
    }
}

static void warmup(struct bench_context *context, statx_call call,
                   unsigned long count) {
    for (unsigned long index = 0; index < count; ++index) {
        (void)call(context, index);
    }
}

static void run_case(const char *name, struct bench_context *context,
                     statx_call call, unsigned long iterations) {
    unsigned long errors = 0;
    int last_errno = 0;
    uint64_t start_ns;
    uint64_t end_ns;

    warmup(context, call, DEFAULT_WARMUP);
    start_ns = monotonic_ns();
    for (unsigned long index = 0; index < iterations; ++index) {
        long result = call(context, index);

        if (result != 0) {
            ++errors;
            last_errno = errno;
        }
    }
    end_ns = monotonic_ns();

    printf("STATX_RESULT case=%s iterations=%lu errors=%lu total_ns=%" PRIu64
           " ns_per_call=%.3f last_errno=%d\n",
           name, iterations, errors, end_ns - start_ns,
           (double)(end_ns - start_ns) / (double)iterations, last_errno);
    if (errors != 0) {
        fprintf(stderr, "%s: %lu failures, last errno=%d (%s)\n", name,
                errors, last_errno, strerror(last_errno));
    }
}

static void free_context(struct bench_context *context) {
    if (context->file_fd >= 0) {
        (void)close(context->file_fd);
    }
    if (context->dir_fd >= 0) {
        (void)close(context->dir_fd);
    }
    for (unsigned long index = 0; index < context->file_count; ++index) {
        free(context->paths[index]);
    }
    free(context->paths);
}

int main(int argc, char **argv) {
    struct bench_context context = {
        .mask = 0,
        .file_count = 0,
        .file_fd = -1,
        .dir_fd = -1,
        .paths = NULL,
        .output = {0},
    };
    unsigned long iterations;
    unsigned long files;
    unsigned long passes;
    unsigned mask;

    parse_options(argc, argv, &iterations, &files, &passes, &mask);
    if (passes > MAX_ITERATIONS / files) {
        fprintf(stderr, "files * passes exceeds the iteration limit\n");
        return 2;
    }
    context.mask = mask;
    context.file_count = files;
    prepare_files(&context);

    printf("STATX_SETUP mask=0x%x fixed_path=%s files=%lu passes=%lu\n", mask,
           FIXED_PATH, files, passes);
    run_case("path_fixed", &context, call_fixed_path, iterations);
    run_case("empty_path", &context, call_empty_path, iterations);
    run_case("relative_dirfd", &context, call_relative_dirfd, iterations);
    run_case("path_rotate_first", &context, call_rotating_path, files);
    run_case("path_rotate_repeat", &context, call_rotating_path,
             files * passes);

    free_context(&context);
    return 0;
}
