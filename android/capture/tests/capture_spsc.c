/* tests/capture_spsc.c — host tests for the shared SPSC capture core.
 * Written before src/spsc.c exists: the initial run must fail at link with
 * EXPECTED_MISSING_IMPLEMENTATION, which is not a functional RED.
 * Every case is independent: own ring, own buffers, cleanup at "done".
 */
#define _POSIX_C_SOURCE 200809L

#include "capture-spsc.h"

#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

#define CAP OPENRELAY_CAPTURE_RING_SAMPLES      /* 960 */
#define MAXREAD OPENRELAY_CAPTURE_FRAME_SAMPLES /* 240 */
#define CANARY ((int16_t)0x5A5A)

/* ------------------------------------------------------------------ */
/* allocation counters: only the core operations are measured          */
/* ------------------------------------------------------------------ */
#ifdef OPENRELAY_WRAP_ALLOC
extern void *__real_malloc(size_t);
extern void *__real_calloc(size_t, size_t);
extern void *__real_realloc(void *, size_t);
extern void __real_free(void *);

static unsigned long g_allocs;
static unsigned long g_frees;

void *__wrap_malloc(size_t n) { g_allocs++; return __real_malloc(n); }
void *__wrap_calloc(size_t a, size_t b) { g_allocs++; return __real_calloc(a, b); }
void *__wrap_realloc(void *p, size_t n) { g_allocs++; return __real_realloc(p, n); }
void __wrap_free(void *p) { g_frees++; __real_free(p); }
#endif

/* ------------------------------------------------------------------ */
/* harness                                                             */
/* ------------------------------------------------------------------ */
static int g_case_failed;
static unsigned long g_checks;
static unsigned long g_asserts_failed;

static void fail_i32(int line, const char *expr, long got, long want) {
    g_case_failed = 1;
    g_asserts_failed++;
    fprintf(stderr, "    line %d: %s -> %ld, want %ld\n", line, expr, got, want);
}

static void fail_u32(int line, const char *expr, unsigned long got,
                     unsigned long want) {
    g_case_failed = 1;
    g_asserts_failed++;
    fprintf(stderr, "    line %d: %s -> %lu, want %lu\n", line, expr, got, want);
}

static void fail_true(int line, const char *expr) {
    g_case_failed = 1;
    g_asserts_failed++;
    fprintf(stderr, "    line %d: %s is false\n", line, expr);
}

#define REQUIRE_I32(expr, want)                                                \
    do {                                                                       \
        g_checks++;                                                            \
        long got_ = (long)(expr);                                              \
        if (got_ != (long)(want)) {                                            \
            fail_i32(__LINE__, #expr, got_, (long)(want));                     \
            goto done;                                                         \
        }                                                                      \
    } while (0)

#define REQUIRE_U32(expr, want)                                                \
    do {                                                                       \
        g_checks++;                                                            \
        unsigned long got_ = (unsigned long)(expr);                            \
        if (got_ != (unsigned long)(want)) {                                   \
            fail_u32(__LINE__, #expr, got_, (unsigned long)(want));            \
            goto done;                                                         \
        }                                                                      \
    } while (0)

#define REQUIRE_TRUE(expr)                                                     \
    do {                                                                       \
        g_checks++;                                                            \
        if (!(expr)) {                                                         \
            fail_true(__LINE__, #expr);                                        \
            goto done;                                                         \
        }                                                                      \
    } while (0)

static void pattern(int16_t *b, uint32_t n, uint32_t base) {
    for (uint32_t i = 0; i < n; i++) {
        b[i] = (int16_t)((base + i) & 0x7FFFu);
    }
}

static void canary_fill(int16_t *b, uint32_t n) {
    for (uint32_t i = 0; i < n; i++) {
        b[i] = CANARY;
    }
}

static int canary_intact(const int16_t *b, uint32_t from, uint32_t n) {
    for (uint32_t i = from; i < n; i++) {
        if (b[i] != CANARY) {
            return 0;
        }
    }
    return 1;
}

static double now_s(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
}

/* ------------------------------------------------------------------ */
/* 1. create / destroy / null                                          */
/* ------------------------------------------------------------------ */
static int t_create_destroy_null(void) {
    openrelay_spsc *r = NULL;
    uint32_t f = 0;

    REQUIRE_I32(openrelay_spsc_create(NULL), OPENRELAY_CAPTURE_ERR_NO_OUTPUT);
    REQUIRE_I32(openrelay_spsc_destroy(NULL), OPENRELAY_CAPTURE_ERR_NULL);

    r = (openrelay_spsc *)(uintptr_t)0x1; /* overwritten on success */
    REQUIRE_I32(openrelay_spsc_create(&r), OPENRELAY_CAPTURE_OK);
    REQUIRE_TRUE(r != NULL);
    REQUIRE_I32(openrelay_spsc_free(r, &f), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(f, CAP);

    REQUIRE_I32(openrelay_spsc_destroy(&r), OPENRELAY_CAPTURE_OK);
    REQUIRE_TRUE(r == NULL);
    /* header: *owner null -> OK */
    REQUIRE_I32(openrelay_spsc_destroy(&r), OPENRELAY_CAPTURE_OK);
    REQUIRE_TRUE(r == NULL);
done:
    if (r != NULL) {
        (void)openrelay_spsc_destroy(&r);
    }
    return g_case_failed;
}

/* ------------------------------------------------------------------ */
/* 2. zero-length operations accept null pointers                      */
/* ------------------------------------------------------------------ */
static int t_zero_len_ops(void) {
    openrelay_spsc *r = NULL;
    openrelay_spsc_push_result out = {7u, 7u};
    int16_t dst[8];
    uint32_t rd = 99u, dr = 99u, f0 = 0, f1 = 0;

    canary_fill(dst, 8);
    REQUIRE_I32(openrelay_spsc_create(&r), OPENRELAY_CAPTURE_OK);
    REQUIRE_I32(openrelay_spsc_free(r, &f0), OPENRELAY_CAPTURE_OK);

    out.written = 7u;
    out.dropped = 7u;
    REQUIRE_I32(openrelay_spsc_push(r, NULL, 0u, &out), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(out.written, 0u);
    REQUIRE_U32(out.dropped, 0u);

    REQUIRE_I32(openrelay_spsc_push(r, dst, 0u, &out), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(out.written, 0u);
    REQUIRE_U32(out.dropped, 0u);

    REQUIRE_I32(openrelay_spsc_read(r, NULL, 0u, &rd), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(rd, 0u);

    rd = 99u;
    REQUIRE_I32(openrelay_spsc_read(r, dst, 0u, &rd), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(rd, 0u);

    dr = 99u;
    REQUIRE_I32(openrelay_spsc_drain(r, 0u, &dr), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(dr, 0u);

    REQUIRE_I32(openrelay_spsc_free(r, &f1), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(f1, f0);
    REQUIRE_TRUE(canary_intact(dst, 0u, 8u));
done:
    if (r != NULL) {
        (void)openrelay_spsc_destroy(&r);
    }
    return g_case_failed;
}

/* ------------------------------------------------------------------ */
/* 3. errors: null outputs and no mutation on failure                  */
/* ------------------------------------------------------------------ */
static int t_errors_no_mutation(void) {
    openrelay_spsc *r = NULL;
    openrelay_spsc_push_result out;
    int16_t src[64];
    int16_t dst[64];
    uint32_t rd, dr, f0 = 0, f1 = 0, v;

    pattern(src, 64u, 0u);
    canary_fill(dst, 64);
    REQUIRE_I32(openrelay_spsc_create(&r), OPENRELAY_CAPTURE_OK);
    REQUIRE_I32(openrelay_spsc_free(r, &f0), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(f0, CAP);

    /* push: out null is ERR_NO_OUTPUT; ring/samples null is ERR_NULL */
    REQUIRE_I32(openrelay_spsc_push(r, src, 10u, NULL), OPENRELAY_CAPTURE_ERR_NO_OUTPUT);
    out.written = 7u;
    out.dropped = 7u;
    REQUIRE_I32(openrelay_spsc_push(NULL, src, 10u, &out), OPENRELAY_CAPTURE_ERR_NULL);
    REQUIRE_U32(out.written, 0u);
    REQUIRE_U32(out.dropped, 0u);
    out.written = 7u;
    out.dropped = 7u;
    REQUIRE_I32(openrelay_spsc_push(r, NULL, 10u, &out), OPENRELAY_CAPTURE_ERR_NULL);
    REQUIRE_U32(out.written, 0u);
    REQUIRE_U32(out.dropped, 0u);

    /* read: out null, caps, nulls; read is reset to 0 on error */
    REQUIRE_I32(openrelay_spsc_read(r, dst, 10u, NULL), OPENRELAY_CAPTURE_ERR_NO_OUTPUT);
    rd = 99u;
    REQUIRE_I32(openrelay_spsc_read(NULL, dst, 10u, &rd), OPENRELAY_CAPTURE_ERR_NULL);
    REQUIRE_U32(rd, 0u);
    REQUIRE_I32(openrelay_spsc_read(r, NULL, 10u, &rd), OPENRELAY_CAPTURE_ERR_NULL);
    REQUIRE_U32(rd, 0u);
    REQUIRE_I32(openrelay_spsc_read(r, dst, MAXREAD + 1u, &rd), OPENRELAY_CAPTURE_ERR_ARGS);
    REQUIRE_U32(rd, 0u);
    REQUIRE_I32(openrelay_spsc_read(r, dst, 0xFFFFFFFFu, &rd), OPENRELAY_CAPTURE_ERR_ARGS);
    REQUIRE_U32(rd, 0u);

    /* drain: dropped null, ring null */
    REQUIRE_I32(openrelay_spsc_drain(r, 10u, NULL), OPENRELAY_CAPTURE_ERR_NO_OUTPUT);
    dr = 99u;
    REQUIRE_I32(openrelay_spsc_drain(NULL, 10u, &dr), OPENRELAY_CAPTURE_ERR_NULL);
    REQUIRE_U32(dr, 0u);

    /* free: out null, ring null (writes *out = 0) */
    REQUIRE_I32(openrelay_spsc_free(r, NULL), OPENRELAY_CAPTURE_ERR_NO_OUTPUT);
    v = 99u;
    REQUIRE_I32(openrelay_spsc_free(NULL, &v), OPENRELAY_CAPTURE_ERR_NULL);
    REQUIRE_U32(v, 0u);

    /* none of the failures may consume or produce samples */
    REQUIRE_I32(openrelay_spsc_free(r, &f1), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(f1, CAP);
    REQUIRE_TRUE(canary_intact(dst, 0u, 64u));
done:
    if (r != NULL) {
        (void)openrelay_spsc_destroy(&r);
    }
    return g_case_failed;
}

/* ------------------------------------------------------------------ */
/* 4. 100 samples, exact FIFO order                                    */
/* ------------------------------------------------------------------ */
static int t_fifo_100(void) {
    openrelay_spsc *r = NULL;
    openrelay_spsc_push_result out;
    int16_t src[100];
    int16_t dst[100];
    uint32_t rd = 0, f = 0;

    pattern(src, 100u, 1000u);
    canary_fill(dst, 100);
    REQUIRE_I32(openrelay_spsc_create(&r), OPENRELAY_CAPTURE_OK);
    REQUIRE_I32(openrelay_spsc_push(r, src, 100u, &out), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(out.written, 100u);
    REQUIRE_U32(out.dropped, 0u);
    REQUIRE_I32(openrelay_spsc_free(r, &f), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(f, CAP - 100u);

    REQUIRE_I32(openrelay_spsc_read(r, dst, 100u, &rd), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(rd, 100u);
    REQUIRE_TRUE(memcmp(src, dst, sizeof(src)) == 0);

    rd = 99u;
    REQUIRE_I32(openrelay_spsc_read(r, dst, 100u, &rd), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(rd, 0u);
    REQUIRE_I32(openrelay_spsc_free(r, &f), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(f, CAP);
done:
    if (r != NULL) {
        (void)openrelay_spsc_destroy(&r);
    }
    return g_case_failed;
}

/* ------------------------------------------------------------------ */
/* 5. overflow: 1024 pushed into 960 -> 960 written + 64 dropped       */
/* ------------------------------------------------------------------ */
static int t_overflow_1024(void) {
    openrelay_spsc *r = NULL;
    openrelay_spsc_push_result out;
    int16_t src[1024];
    int16_t copy[1024];
    int16_t dst[MAXREAD];
    uint32_t rd = 0, f = 0, total = 0;

    pattern(src, 1024u, 0u);
    memcpy(copy, src, sizeof(src));
    REQUIRE_I32(openrelay_spsc_create(&r), OPENRELAY_CAPTURE_OK);
    REQUIRE_I32(openrelay_spsc_push(r, src, 1024u, &out), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(out.written, CAP);
    REQUIRE_U32(out.dropped, 1024u - CAP);
    REQUIRE_TRUE(memcmp(src, copy, sizeof(src)) == 0); /* source untouched */
    REQUIRE_I32(openrelay_spsc_free(r, &f), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(f, 0u);

    /* the ring must hold the FIRST 960 samples, never a discarded tail */
    while (total < CAP) {
        rd = 0;
        REQUIRE_I32(openrelay_spsc_read(r, dst, MAXREAD, &rd), OPENRELAY_CAPTURE_OK);
        REQUIRE_U32(rd, MAXREAD);
        REQUIRE_TRUE(memcmp(dst, copy + total, MAXREAD * sizeof(int16_t)) == 0);
        total += rd;
    }
    rd = 1u;
    REQUIRE_I32(openrelay_spsc_read(r, dst, MAXREAD, &rd), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(rd, 0u);
    REQUIRE_I32(openrelay_spsc_free(r, &f), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(f, CAP);
done:
    if (r != NULL) {
        (void)openrelay_spsc_destroy(&r);
    }
    return g_case_failed;
}

/* ------------------------------------------------------------------ */
/* 6. partial write: free 100, caller has 4096 readable samples        */
/* ------------------------------------------------------------------ */
static int t_partial_free_100(void) {
    openrelay_spsc *r = NULL;
    openrelay_spsc_push_result out;
    int16_t bulk[4096];
    int16_t pre[860];
    int16_t dst[MAXREAD];
    uint32_t rd = 0, f = 0, total = 0;

    pattern(pre, 860u, 0u);
    pattern(bulk, 4096u, 5000u);
    REQUIRE_I32(openrelay_spsc_create(&r), OPENRELAY_CAPTURE_OK);
    REQUIRE_I32(openrelay_spsc_push(r, pre, 860u, &out), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(out.written, 860u);
    REQUIRE_U32(out.dropped, 0u);
    REQUIRE_I32(openrelay_spsc_free(r, &f), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(f, 100u);

    REQUIRE_I32(openrelay_spsc_push(r, bulk, 4096u, &out), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(out.written, 100u);
    REQUIRE_U32(out.dropped, 4096u - 100u);
    REQUIRE_I32(openrelay_spsc_free(r, &f), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(f, 0u);

    /* kept prefix: all 860 old samples then the first 100 of bulk */
    while (total < 860u) {
        uint32_t chunk = 860u - total;
        if (chunk > MAXREAD) {
            chunk = MAXREAD;
        }
        rd = 0;
        REQUIRE_I32(openrelay_spsc_read(r, dst, chunk, &rd), OPENRELAY_CAPTURE_OK);
        REQUIRE_U32(rd, chunk);
        REQUIRE_TRUE(memcmp(dst, pre + total, rd * sizeof(int16_t)) == 0);
        total += rd;
    }
    /* the ring now holds exactly the first 100 samples of bulk, FIFO */
    rd = 0;
    REQUIRE_I32(openrelay_spsc_read(r, dst, MAXREAD, &rd), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(rd, CAP - 860u);
    REQUIRE_TRUE(memcmp(dst, bulk, (CAP - 860u) * sizeof(int16_t)) == 0);
    rd = 1u;
    REQUIRE_I32(openrelay_spsc_read(r, dst, MAXREAD, &rd), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(rd, 0u);
done:
    if (r != NULL) {
        (void)openrelay_spsc_destroy(&r);
    }
    return g_case_failed;
}

/* ------------------------------------------------------------------ */
/* 7. read cap 240, short read of 100, canary past the copied range    */
/* ------------------------------------------------------------------ */
static int t_read_cap_and_canary(void) {
    openrelay_spsc *r = NULL;
    openrelay_spsc_push_result out;
    int16_t src[340];
    int16_t dst[MAXREAD + 32u];
    uint32_t rd = 0;

    pattern(src, 340u, 0u);
    canary_fill(dst, MAXREAD + 32u);
    REQUIRE_I32(openrelay_spsc_create(&r), OPENRELAY_CAPTURE_OK);
    REQUIRE_I32(openrelay_spsc_push(r, src, 340u, &out), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(out.written, 340u);

    REQUIRE_I32(openrelay_spsc_read(r, dst, MAXREAD, &rd), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(rd, MAXREAD);
    REQUIRE_TRUE(memcmp(dst, src, MAXREAD * sizeof(int16_t)) == 0);
    REQUIRE_TRUE(canary_intact(dst, MAXREAD, MAXREAD + 32u));

    canary_fill(dst, MAXREAD + 32u);
    REQUIRE_I32(openrelay_spsc_read(r, dst, MAXREAD, &rd), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(rd, 100u);
    REQUIRE_TRUE(memcmp(dst, src + MAXREAD, 100u * sizeof(int16_t)) == 0);
    REQUIRE_TRUE(canary_intact(dst, 100u, MAXREAD + 32u));

    rd = 5u;
    REQUIRE_I32(openrelay_spsc_read(r, dst, MAXREAD, &rd), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(rd, 0u);
done:
    if (r != NULL) {
        (void)openrelay_spsc_destroy(&r);
    }
    return g_case_failed;
}

/* ------------------------------------------------------------------ */
/* 8. wrap: push/read cycles that cross the end of the ring            */
/* ------------------------------------------------------------------ */
static int t_wrap_cycles(void) {
    openrelay_spsc *r = NULL;
    openrelay_spsc_push_result out;
    int16_t src[700];
    int16_t dst[MAXREAD];
    uint32_t rd = 0, expected = 0;

    REQUIRE_I32(openrelay_spsc_create(&r), OPENRELAY_CAPTURE_OK);
    for (uint32_t cycle = 0; cycle < 12u; cycle++) {
        uint32_t n = 500u + (cycle % 3u) * 100u; /* 500..700 */
        pattern(src, n, expected);
        REQUIRE_I32(openrelay_spsc_push(r, src, n, &out), OPENRELAY_CAPTURE_OK);
        REQUIRE_U32(out.written, n);
        REQUIRE_U32(out.dropped, 0u);
        uint32_t got = 0;
        while (got < n) {
            rd = 0;
            REQUIRE_I32(openrelay_spsc_read(r, dst, MAXREAD, &rd), OPENRELAY_CAPTURE_OK);
            REQUIRE_TRUE(rd > 0u);
            for (uint32_t i = 0; i < rd; i++) {
                REQUIRE_TRUE(dst[i] == (int16_t)((expected + got + i) & 0x7FFFu));
            }
            got += rd;
        }
        expected += n;
    }
done:
    if (r != NULL) {
        (void)openrelay_spsc_destroy(&r);
    }
    return g_case_failed;
}

/* ------------------------------------------------------------------ */
/* 9. fixed capacity: no growth beyond 960                             */
/* ------------------------------------------------------------------ */
static int t_fixed_capacity(void) {
    openrelay_spsc *r = NULL;
    openrelay_spsc_push_result out;
    int16_t src[CAP + 8u];
    int16_t dst[MAXREAD];
    uint32_t rd = 0, f = 0;

    pattern(src, CAP + 8u, 0u);
    REQUIRE_I32(openrelay_spsc_create(&r), OPENRELAY_CAPTURE_OK);
    REQUIRE_I32(openrelay_spsc_free(r, &f), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(f, CAP); /* capacity is fixed by create's signature */

    REQUIRE_I32(openrelay_spsc_push(r, src, CAP, &out), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(out.written, CAP);
    REQUIRE_U32(out.dropped, 0u);
    REQUIRE_I32(openrelay_spsc_free(r, &f), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(f, 0u);

    REQUIRE_I32(openrelay_spsc_push(r, src, 8u, &out), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(out.written, 0u);
    REQUIRE_U32(out.dropped, 8u);

    for (uint32_t i = 0; i < CAP / MAXREAD; i++) {
        rd = 0;
        REQUIRE_I32(openrelay_spsc_read(r, dst, MAXREAD, &rd), OPENRELAY_CAPTURE_OK);
        REQUIRE_U32(rd, MAXREAD);
    }
    REQUIRE_I32(openrelay_spsc_free(r, &f), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(f, CAP);
done:
    if (r != NULL) {
        (void)openrelay_spsc_destroy(&r);
    }
    return g_case_failed;
}

/* ------------------------------------------------------------------ */
/* 10. the source buffer is never written                              */
/* ------------------------------------------------------------------ */
static int t_source_immutable(void) {
    openrelay_spsc *r = NULL;
    openrelay_spsc_push_result out;
    int16_t src[1024];
    int16_t copy[1024];
    uint32_t rd = 0;
    int16_t dst[MAXREAD];

    pattern(src, 1024u, 321u);
    memcpy(copy, src, sizeof(src));
    REQUIRE_I32(openrelay_spsc_create(&r), OPENRELAY_CAPTURE_OK);
    REQUIRE_I32(openrelay_spsc_push(r, src, 1024u, &out), OPENRELAY_CAPTURE_OK);
    REQUIRE_TRUE(memcmp(src, copy, sizeof(src)) == 0);
    /* and after a consuming read the caller's buffer is still intact */
    REQUIRE_I32(openrelay_spsc_read(r, dst, MAXREAD, &rd), OPENRELAY_CAPTURE_OK);
    REQUIRE_TRUE(memcmp(src, copy, sizeof(src)) == 0);
done:
    if (r != NULL) {
        (void)openrelay_spsc_destroy(&r);
    }
    return g_case_failed;
}

/* ------------------------------------------------------------------ */
/* 11. free() is coherent with what read/drain actually did            */
/* ------------------------------------------------------------------ */
static int t_free_coherent(void) {
    openrelay_spsc *r = NULL;
    openrelay_spsc_push_result out;
    int16_t src[CAP];
    int16_t dst[MAXREAD];
    uint32_t rd = 0, dr = 0, f = 0, occ = 0;

    pattern(src, CAP, 0u);
    REQUIRE_I32(openrelay_spsc_create(&r), OPENRELAY_CAPTURE_OK);

    REQUIRE_I32(openrelay_spsc_push(r, src, 500u, &out), OPENRELAY_CAPTURE_OK);
    occ = 500u;
    REQUIRE_I32(openrelay_spsc_free(r, &f), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(f, CAP - occ);

    REQUIRE_I32(openrelay_spsc_read(r, dst, MAXREAD, &rd), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(rd, MAXREAD);
    occ -= MAXREAD;
    REQUIRE_I32(openrelay_spsc_free(r, &f), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(f, CAP - occ);

    REQUIRE_I32(openrelay_spsc_drain(r, 100u, &dr), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(dr, 100u);
    occ -= 100u;
    REQUIRE_I32(openrelay_spsc_free(r, &f), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(f, CAP - occ);

    /* drain clamps at what is available and never goes negative */
    dr = 0;
    REQUIRE_I32(openrelay_spsc_drain(r, 100000u, &dr), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(dr, occ);
    REQUIRE_I32(openrelay_spsc_free(r, &f), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(f, CAP);

    /* free() does not consume */
    REQUIRE_I32(openrelay_spsc_push(r, src, 300u, &out), OPENRELAY_CAPTURE_OK);
    REQUIRE_I32(openrelay_spsc_free(r, &f), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(f, CAP - 300u);
    REQUIRE_I32(openrelay_spsc_free(r, &f), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(f, CAP - 300u);
done:
    if (r != NULL) {
        (void)openrelay_spsc_destroy(&r);
    }
    return g_case_failed;
}

/* ------------------------------------------------------------------ */
/* 12. thousands of indexed cycles                                     */
/* ------------------------------------------------------------------ */
static int t_long_run_indexed(void) {
    openrelay_spsc *r = NULL;
    openrelay_spsc_push_result out;
    int16_t src[37];
    int16_t dst[37];
    uint32_t rd = 0, expected = 0;

    REQUIRE_I32(openrelay_spsc_create(&r), OPENRELAY_CAPTURE_OK);
    for (uint32_t cycle = 0; cycle < 2000u; cycle++) {
        pattern(src, 37u, expected);
        REQUIRE_I32(openrelay_spsc_push(r, src, 37u, &out), OPENRELAY_CAPTURE_OK);
        REQUIRE_U32(out.written, 37u);
        REQUIRE_U32(out.dropped, 0u);
        rd = 0;
        REQUIRE_I32(openrelay_spsc_read(r, dst, 37u, &rd), OPENRELAY_CAPTURE_OK);
        REQUIRE_U32(rd, 37u);
        REQUIRE_TRUE(memcmp(dst, src, sizeof(src)) == 0);
        expected += 37u;
    }
done:
    if (r != NULL) {
        (void)openrelay_spsc_destroy(&r);
    }
    return g_case_failed;
}

/* ------------------------------------------------------------------ */
/* 13. no alloc/free inside push/read/drain/free                       */
/* ------------------------------------------------------------------ */
static int t_no_alloc_in_core(void) {
#ifdef OPENRELAY_WRAP_ALLOC
    openrelay_spsc *r = NULL;
    openrelay_spsc_push_result out;
    int16_t src[CAP];
    int16_t dst[MAXREAD];
    uint32_t rd = 0, dr = 0, f = 0;
    unsigned long a0, f0;

    pattern(src, CAP, 0u);
    REQUIRE_I32(openrelay_spsc_create(&r), OPENRELAY_CAPTURE_OK);
    /* create may allocate: only the core calls below are measured */

    a0 = g_allocs;
    f0 = g_frees;
    REQUIRE_I32(openrelay_spsc_push(r, src, 1024u, &out), OPENRELAY_CAPTURE_OK);
    REQUIRE_I32(openrelay_spsc_read(r, dst, MAXREAD, &rd), OPENRELAY_CAPTURE_OK);
    REQUIRE_I32(openrelay_spsc_drain(r, 10u, &dr), OPENRELAY_CAPTURE_OK);
    REQUIRE_I32(openrelay_spsc_free(r, &f), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(g_allocs - a0, 0u);
    REQUIRE_U32(g_frees - f0, 0u);
done:
    if (r != NULL) {
        (void)openrelay_spsc_destroy(&r);
    }
    return g_case_failed;
#else
    printf("[SKIP] no_alloc_in_core (built without --wrap=malloc)\n");
    return 0;
#endif
}

/* ------------------------------------------------------------------ */
/* 14. atomics used by the core are lock-free on this host             */
/* ------------------------------------------------------------------ */
static int t_atomics_lockfree(void) {
    _Atomic uint32_t a = 0;
    _Atomic uint32_t b = 0;

    REQUIRE_TRUE(atomic_is_lock_free(&a));
    REQUIRE_TRUE(atomic_is_lock_free(&b));
    REQUIRE_U32(atomic_load_explicit(&a, memory_order_acquire), 0u);
    atomic_store_explicit(&a, 7u, memory_order_release);
    REQUIRE_TRUE(atomic_exchange_explicit(&b, atomic_load_explicit(&a, memory_order_acquire),
                                          memory_order_acq_rel) == 0u);
    REQUIRE_U32(atomic_load_explicit(&b, memory_order_acquire), 7u);
done:
    return g_case_failed;
}

/* ------------------------------------------------------------------ */
/* 15. real producer/consumer stress                                   */
/* ------------------------------------------------------------------ */
#ifndef OPENRELAY_STRESS_SCALE
#define OPENRELAY_STRESS_SCALE 1
#endif

#define STRESS_A_BLOCKS (500u * OPENRELAY_STRESS_SCALE)
#define STRESS_A_BLOCK 240u
#define STRESS_B_ITERS (200u * OPENRELAY_STRESS_SCALE)
#define STRESS_B_BLOCK 1024u
#define STRESS_DEADLINE_S 15.0

struct stress_a {
    openrelay_spsc *ring;
    uint32_t blocks;
    _Atomic uint32_t done;
    _Atomic uint32_t failure;
};

static void *stress_a_producer(void *arg) {
    struct stress_a *s = arg;
    int16_t buf[STRESS_A_BLOCK];
    openrelay_spsc_push_result out;
    uint32_t pushed = 0;
    double deadline = now_s() + STRESS_DEADLINE_S;

    for (uint32_t i = 0; i < s->blocks; i++) {
        /* throttle so this phase never drops: ordering can be checked exactly */
        for (;;) {
            uint32_t f = 0;
            if (openrelay_spsc_free(s->ring, &f) != OPENRELAY_CAPTURE_OK) {
                atomic_store_explicit(&s->failure, 1u, memory_order_release);
                return NULL;
            }
            if (f >= STRESS_A_BLOCK) {
                break;
            }
            if (now_s() > deadline) {
                atomic_store_explicit(&s->failure, 4u, memory_order_release);
                return NULL;
            }
            sched_yield();
        }
        for (uint32_t k = 0; k < STRESS_A_BLOCK; k++) {
            buf[k] = (int16_t)((pushed + k) & 0x7FFFu);
        }
        if (openrelay_spsc_push(s->ring, buf, STRESS_A_BLOCK, &out) != OPENRELAY_CAPTURE_OK ||
            out.written != STRESS_A_BLOCK || out.dropped != 0u) {
            atomic_store_explicit(&s->failure, 1u, memory_order_release);
            return NULL;
        }
        pushed += STRESS_A_BLOCK;
    }
    atomic_store_explicit(&s->done, 1u, memory_order_release);
    return NULL;
}

static void *stress_a_consumer(void *arg) {
    struct stress_a *s = arg;
    int16_t buf[MAXREAD];
    uint32_t expected = 0;
    double deadline = now_s() + STRESS_DEADLINE_S;

    for (;;) {
        uint32_t rd = 0;
        if (openrelay_spsc_read(s->ring, buf, MAXREAD, &rd) != OPENRELAY_CAPTURE_OK) {
            atomic_store_explicit(&s->failure, 1u, memory_order_release);
            return NULL;
        }
        for (uint32_t i = 0; i < rd; i++) {
            if (buf[i] != (int16_t)((expected + i) & 0x7FFFu)) {
                atomic_store_explicit(&s->failure, 1u, memory_order_release);
                return NULL;
            }
        }
        expected += rd;
        if (rd == 0u) {
            if (atomic_load_explicit(&s->done, memory_order_acquire)) {
                /* drain whatever the last block left behind, then stop */
                for (;;) {
                    uint32_t tail = 0;
                    if (openrelay_spsc_read(s->ring, buf, MAXREAD, &tail) != OPENRELAY_CAPTURE_OK) {
                        atomic_store_explicit(&s->failure, 1u,
                                             memory_order_release);
                        return NULL;
                    }
                    for (uint32_t i = 0; i < tail; i++) {
                        if (buf[i] != (int16_t)((expected + i) & 0x7FFFu)) {
                            atomic_store_explicit(&s->failure, 1u,
                                                 memory_order_release);
                            return NULL;
                        }
                    }
                    expected += tail;
                    if (tail == 0u) {
                        break;
                    }
                }
                break;
            }
            if (now_s() > deadline) {
                atomic_store_explicit(&s->failure, 2u, memory_order_release);
                return NULL;
            }
            sched_yield();
        }
    }
    if (expected != s->blocks * STRESS_A_BLOCK) {
        atomic_store_explicit(&s->failure, 3u, memory_order_release);
    }
    return NULL;
}

static int t_stress_threads(void) {
    openrelay_spsc *r = NULL;
    pthread_t prod, cons;
    struct stress_a s;
    int16_t src[STRESS_B_BLOCK];
    openrelay_spsc_push_result out;
    uint32_t written = 0, dropped = 0, read_total = 0;
    uint32_t f = 0;
    double t0, elapsed;

    /* phase A: throttled producer, exact order check, zero drops */
    memset(&s, 0, sizeof(s));
    REQUIRE_I32(openrelay_spsc_create(&r), OPENRELAY_CAPTURE_OK);
    s.ring = r;
    s.blocks = STRESS_A_BLOCKS;
    atomic_store_explicit(&s.done, 0u, memory_order_relaxed);
    atomic_store_explicit(&s.failure, 0u, memory_order_relaxed);
    t0 = now_s();
    REQUIRE_I32(pthread_create(&prod, NULL, stress_a_producer, &s), 0);
    REQUIRE_I32(pthread_create(&cons, NULL, stress_a_consumer, &s), 0);
    REQUIRE_I32(pthread_join(prod, NULL), 0);
    REQUIRE_I32(pthread_join(cons, NULL), 0);
    elapsed = now_s() - t0;
    REQUIRE_U32(atomic_load_explicit(&s.failure, memory_order_acquire), 0u);
    REQUIRE_TRUE(elapsed < STRESS_DEADLINE_S);
    REQUIRE_I32(openrelay_spsc_free(r, &f), OPENRELAY_CAPTURE_OK);
    REQUIRE_U32(f, CAP);
    REQUIRE_I32(openrelay_spsc_destroy(&r), OPENRELAY_CAPTURE_OK);

    /* phase B: unthrottled producer, drop accounting must close exactly */
    REQUIRE_I32(openrelay_spsc_create(&r), OPENRELAY_CAPTURE_OK);
    for (uint32_t i = 0; i < STRESS_B_ITERS; i++) {
        pattern(src, STRESS_B_BLOCK, i * STRESS_B_BLOCK);
        REQUIRE_I32(openrelay_spsc_push(r, src, STRESS_B_BLOCK, &out), OPENRELAY_CAPTURE_OK);
        REQUIRE_U32(out.written + out.dropped, STRESS_B_BLOCK);
        written += out.written;
        dropped += out.dropped;
        /* consumer side runs inline here: read what fits */
        for (;;) {
            uint32_t rd = 0;
            int16_t dst[MAXREAD];
            REQUIRE_I32(openrelay_spsc_read(r, dst, MAXREAD, &rd), OPENRELAY_CAPTURE_OK);
            read_total += rd;
            if (rd == 0u) {
                break;
            }
        }
    }
    for (;;) {
        uint32_t rd = 0;
        int16_t dst[MAXREAD];
        REQUIRE_I32(openrelay_spsc_read(r, dst, MAXREAD, &rd), OPENRELAY_CAPTURE_OK);
        read_total += rd;
        if (rd == 0u) {
            break;
        }
    }
    REQUIRE_U32(read_total, written);
    REQUIRE_U32(written + dropped, STRESS_B_ITERS * STRESS_B_BLOCK);
    printf("    phase A: %u blocks ordered, phase B: written=%u dropped=%u\n",
           STRESS_A_BLOCKS, written, dropped);
done:
    if (r != NULL) {
        (void)openrelay_spsc_destroy(&r);
    }
    return g_case_failed;
}

/* ------------------------------------------------------------------ */
/* registry                                                            */
/* ------------------------------------------------------------------ */
struct tcase {
    const char *name;
    int (*fn)(void);
};

static const struct tcase k_cases[] = {
    {"create_destroy_null", t_create_destroy_null},
    {"zero_len_ops", t_zero_len_ops},
    {"errors_no_mutation", t_errors_no_mutation},
    {"fifo_100", t_fifo_100},
    {"overflow_1024", t_overflow_1024},
    {"partial_free_100", t_partial_free_100},
    {"read_cap_and_canary", t_read_cap_and_canary},
    {"wrap_cycles", t_wrap_cycles},
    {"fixed_capacity", t_fixed_capacity},
    {"source_immutable", t_source_immutable},
    {"free_coherent", t_free_coherent},
    {"long_run_indexed", t_long_run_indexed},
    {"no_alloc_in_core", t_no_alloc_in_core},
    {"atomics_lockfree", t_atomics_lockfree},
    {"stress_threads", t_stress_threads},
};

int main(void) {
    unsigned long pass = 0, fail = 0;
    int failed = 0;

#ifdef __ATOMIC_INT_LOCK_FREE
    printf("ATOMIC_INT_LOCK_FREE=%d ATOMIC_LLONG_LOCK_FREE=%d sizeof(void*)=%u\n",
           __ATOMIC_INT_LOCK_FREE, __ATOMIC_LLONG_LOCK_FREE,
           (unsigned)sizeof(void *));
#endif
    printf("cases=%u\n", (unsigned)(sizeof(k_cases) / sizeof(k_cases[0])));

    for (unsigned i = 0; i < sizeof(k_cases) / sizeof(k_cases[0]); i++) {
        g_case_failed = 0;
        int rc = k_cases[i].fn();
        if (rc == 0) {
            pass++;
            printf("[PASS] %s\n", k_cases[i].name);
        } else {
            fail++;
            failed = 1;
            printf("[FAIL] %s\n", k_cases[i].name);
        }
        fflush(stdout);
    }

    printf("summary: pass=%lu fail=%lu checks=%lu failed_asserts=%lu\n", pass,
           fail, g_checks, g_asserts_failed);
    return failed ? 1 : 0;
}
