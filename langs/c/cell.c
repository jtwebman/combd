// The combd test cell in C (wasi-libc, reactor).
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

__attribute__((import_module("combd"), import_name("counter_incr"))) int64_t counter_incr(void);

static uint64_t fib(uint32_t n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }

__attribute__((export_name("alloc"))) void *cell_alloc(uint32_t len) { return malloc(len ? len : 1); }

// The host calls this after reading an answer.
__attribute__((export_name("release"))) void cell_release(char *p, uint32_t len) { free(p); }

// Returns the response as (ptr << 32) | len.
__attribute__((export_name("handle"))) uint64_t handle(char *req, uint32_t len) {
    char cmd[64] = {0};
    memcpy(cmd, req, len < 63 ? len : 63);
    free(req);
    char *out = malloc(64);
    int n;
    if (strcmp(cmd, "hello") == 0) {
        n = snprintf(out, 64, "hello #%lld", (long long)counter_incr());
    } else if (strncmp(cmd, "fib ", 4) == 0) {
        n = snprintf(out, 64, "%llu", (unsigned long long)fib((uint32_t)atoi(cmd + 4)));
    } else if (strcmp(cmd, "print") == 0) {
        printf("hi from c\n");
        fflush(stdout);
        n = snprintf(out, 64, "printed");
    } else if (strcmp(cmd, "spin") == 0) {
        for (volatile uint64_t x = 0;; x++) {
        }
    } else if (strcmp(cmd, "panic") == 0) {
        __builtin_trap();
    } else {
        n = snprintf(out, 64, "unknown");
    }
    return ((uint64_t)(uintptr_t)out << 32) | (uint32_t)n;
}
