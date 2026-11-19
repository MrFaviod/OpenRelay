/* tests/capture_spsc_counter_scenarios.c — counter-wrap scenarios, separate
 * from the byte-exact parent repro (capture_spsc_counter_wrap.c) and from the
 * 15 original cases. Same single implementation: src/spsc.c. No second core,
 * no fake RED, no private state: everything here goes through the public API.
 *
 * What each scenario actually reaches:
 *   S1  positive control: the second push ends exactly at the 2^32 boundary;
 *       four reads consume its block, ending at the boundary as well.
 *       This case passes on the pre-fix core too.
 *   S2  positive control: push and drain cross the wrap, but the 912 pushed
 *       are drained before the next push, so the ring is empty across the
 *       wrap and no payload survives it
 *   S3  the only case in this file that reproduces the bug: 100+40+60 pushed
 *       across the wrap and read back in 16-sample windows
 *   S4  positive control: free() coherence on both sides of the wrap;
 *       occupancy only, no payload is read
 *   S5  positive control: many laps of the 960 slots with the counters low,
 *       never approaching 2^32
 *
 * 2^32 % 960 == 256, so a slot index derived as counter % 960 lands 256 slots
 * away from the true position once the counter passes the wrap. Only S3
 * exercises that transition with a payload in flight. The residual-occupancy
 * hole (payload left in the ring across the wrap, a partial drain, a further
 * push) is covered by tests/capture_spsc_residual.c, not here.
 */
#include "capture-spsc.h"

#include <stdint.h>
#include <stdio.h>
#include <string.h>

#define CAP OPENRELAY_CAPTURE_RING_SAMPLES /* 960 */
#define MAXREAD OPENRELAY_CAPTURE_FRAME_SAMPLES /* 240 */

static unsigned long g_fail;

static void bad(const char *what, unsigned long a, unsigned long b) {
    g_fail++;
    fprintf(stderr, "    %s: got %lu want %lu\n", what, a, b);
}

#define CK(cond, what, got, want)                                              \
    do {                                                                       \
        if (!(cond)) {                                                         \
            bad(what, (unsigned long)(got), (unsigned long)(want));            \
        }                                                                      \
    } while (0)

/* Fast-forward the public counters to `target` samples of cumulative push,
 * draining as we go so the ring is always empty between blocks. The parent
 * repro does the same thing; this is seconds, not hours. */
static int fast_forward(openrelay_spsc *r, uint32_t target, int16_t *filler) {
    uint64_t left = target;
    openrelay_spsc_push_result p;
    uint32_t d = 0;

    while (left) {
        uint32_t n = left > CAP ? CAP : (uint32_t)left;
        if (openrelay_spsc_push(r, filler, n, &p) != OPENRELAY_CAPTURE_OK) {
            return 1;
        }
        if (p.written != n || p.dropped != 0u) {
            return 1;
        }
        if (openrelay_spsc_drain(r, n, &d) != OPENRELAY_CAPTURE_OK || d != n) {
            return 1;
        }
        left -= n;
    }
    return 0;
}

/* S1: fill exactly to capacity; the final 240-sample read ends at the counter
 * boundary, rather than crossing it with remaining payload.
 * Positive control: this case passes on the pre-fix core as well. */
static void s1_read_across_wrap(void) {
    openrelay_spsc *r = NULL;
    int16_t filler[CAP], src[CAP], dst[MAXREAD];
    uint32_t f = 0, rd = 0, total = 0;

    /* land 1920 before the wrap: two full pushes reach exactly 2^32 */
    const uint32_t target = (uint32_t)(0u - 1920u); /* 2^32 - 1920 */

    for (uint32_t i = 0; i < CAP; i++) {
        filler[i] = (int16_t)(-6000 + (int)i);
    }
    if (openrelay_spsc_create(&r) != OPENRELAY_CAPTURE_OK) {
        bad("s1 create", 1, 0);
        return;
    }
    if (fast_forward(r, target, filler) != 0) {
        bad("s1 fast_forward", 1, 0);
        goto done;
    }
    CK(openrelay_spsc_free(r, &f) == 0 && f == CAP, "s1 free before", f, CAP);

    for (uint32_t i = 0; i < CAP; i++) {
        src[i] = (int16_t)((700 + i) & 0x7FFFu);
    }
    /* push 1920 in two 960 blocks, draining nothing: the second push crosses 2^32 */
    openrelay_spsc_push_result p;
    int16_t tmp[CAP];
    for (uint32_t blk = 0; blk < 2; blk++) {
        for (uint32_t i = 0; i < CAP; i++) {
            tmp[i] = (int16_t)((src[i] + (int16_t)(blk * 3)) & 0x7FFFu);
        }
        /* keep the ring able to accept a full block: drain what we pushed */
        if (blk == 1) {
            /* the second block is what crosses the wrap; read it all back */
            if (openrelay_spsc_push(r, tmp, CAP, &p) != 0 || p.written != CAP) {
                bad("s1 push2", p.written, CAP);
                goto done;
            }
            while (total < CAP) {
                rd = 0;
                if (openrelay_spsc_read(r, dst, MAXREAD, &rd) != 0) {
                    bad("s1 read rc", 1, 0);
                    goto done;
                }
                if (rd == 0u) {
                    break;
                }
                if (memcmp(dst, tmp + total, rd * sizeof(int16_t)) != 0) {
                    bad("s1 fifo mismatch at", total, total);
                    goto done;
                }
                total += rd;
            }
            CK(total == CAP, "s1 total", total, CAP);
        } else {
            if (openrelay_spsc_push(r, tmp, CAP, &p) != 0 || p.written != CAP) {
                bad("s1 push1", p.written, CAP);
                goto done;
            }
            uint32_t d = 0;
            if (openrelay_spsc_drain(r, CAP, &d) != 0 || d != CAP) {
                bad("s1 drain1", d, CAP);
                goto done;
            }
        }
    }
done:
    (void)openrelay_spsc_destroy(&r);
}

/* S2: the drain itself is what crosses the wrap; the following read must
 * still see the samples that were pushed after the drain, in FIFO order.
 * Positive control: the 912 just pushed are drained before the next push, so
 * the ring is empty across the wrap and this case passes on the pre-fix core. */
static void s2_drain_across_wrap(void) {
    openrelay_spsc *r = NULL;
    int16_t filler[CAP], a[CAP], b[CAP], dst[MAXREAD];
    uint32_t f = 0, d = 0, rd = 0, total = 0;
    const uint32_t target = (uint32_t)(0u - 912u); /* 2^32 - 912 */

    for (uint32_t i = 0; i < CAP; i++) {
        filler[i] = (int16_t)(-6000 + (int)i);
    }
    if (openrelay_spsc_create(&r) != OPENRELAY_CAPTURE_OK) {
        bad("s2 create", 1, 0);
        return;
    }
    if (fast_forward(r, target, filler) != 0) {
        bad("s2 fast_forward", 1, 0);
        goto done;
    }
    for (uint32_t i = 0; i < CAP; i++) {
        a[i] = (int16_t)((300 + i) & 0x7FFFu);
        b[i] = (int16_t)((900 + i) & 0x7FFFu);
    }
    openrelay_spsc_push_result p;
    /* 912 + 960 crosses 2^32 inside the push; drain 912 next (still crossing) */
    if (openrelay_spsc_push(r, a, 912u, &p) != 0 || p.written != 912u) {
        bad("s2 push a", p.written, 912);
        goto done;
    }
    if (openrelay_spsc_free(r, &f) != 0 || f != CAP - 912u) {
        bad("s2 free mid", f, CAP - 912u);
        goto done;
    }
    if (openrelay_spsc_drain(r, 912u, &d) != 0 || d != 912u) {
        bad("s2 drain", d, 912);
        goto done;
    }
    CK(openrelay_spsc_free(r, &f) == 0 && f == CAP, "s2 free after drain", f, CAP);

    /* push a full block after the wrap and read it back in order */
    if (openrelay_spsc_push(r, b, CAP, &p) != 0 || p.written != CAP) {
        bad("s2 push b", p.written, CAP);
        goto done;
    }
    while (total < CAP) {
        rd = 0;
        if (openrelay_spsc_read(r, dst, MAXREAD, &rd) != 0) {
            bad("s2 read rc", 1, 0);
            goto done;
        }
        if (rd == 0u) {
            break;
        }
        if (memcmp(dst, b + total, rd * sizeof(int16_t)) != 0) {
            bad("s2 fifo mismatch at", total, total);
            goto done;
        }
        total += rd;
    }
    CK(total == CAP, "s2 total", total, CAP);
done:
    (void)openrelay_spsc_destroy(&r);
}

/* S3: interleaved partial push/read/read-short across the wrap.
 * This is the only scenario in this file that reproduces the bug: it fails on
 * the pre-fix core (first mismatch window at 96 of 200) and passes on
 * src/spsc.c. */
static void s3_interleaved(void) {
    openrelay_spsc *r = NULL;
    int16_t filler[CAP], src[CAP], dst[MAXREAD];
    uint32_t rd = 0, expect = 0;
    const uint32_t target = (uint32_t)(0u - 100u); /* 2^32 - 100 */

    for (uint32_t i = 0; i < CAP; i++) {
        filler[i] = (int16_t)(-6000 + (int)i);
    }
    if (openrelay_spsc_create(&r) != OPENRELAY_CAPTURE_OK) {
        bad("s3 create", 1, 0);
        return;
    }
    if (fast_forward(r, target, filler) != 0) {
        bad("s3 fast_forward", 1, 0);
        goto done;
    }
    for (uint32_t i = 0; i < CAP; i++) {
        src[i] = (int16_t)(i & 0x7FFFu);
    }
    openrelay_spsc_push_result p;
    /* 100 then 40 then 60: 200 samples pushed straddling 2^32 */
    const uint32_t chunks[3] = {100u, 40u, 60u};
    uint32_t off = 0;
    for (int c = 0; c < 3; c++) {
        uint32_t n = chunks[c];
        if (openrelay_spsc_push(r, src + off, n, &p) != 0 || p.written != n) {
            bad("s3 push", p.written, n);
            goto done;
        }
        off += n;
    }
    /* read 16 at a time; the 2^32 boundary sits inside this data */
    while (expect < off) {
        uint32_t want = off - expect;
        if (want > 16u) {
            want = 16u;
        }
        rd = 0;
        if (openrelay_spsc_read(r, dst, want, &rd) != 0) {
            bad("s3 read rc", 1, 0);
            goto done;
        }
        CK(rd == want, "s3 read len", rd, want);
        if (rd == want && memcmp(dst, src + expect, rd * sizeof(int16_t)) != 0) {
            bad("s3 fifo mismatch at", expect, expect);
            goto done;
        }
        expect += rd;
        if (rd == 0u) {
            break;
        }
    }
    CK(expect == off, "s3 total", expect, off);
done:
    (void)openrelay_spsc_destroy(&r);
}

/* S4: free() stays coherent on both sides of the wrap.
 * Positive control: it asserts occupancy only, never a payload, so it passes
 * on the pre-fix core as well. */
static void s4_free_coherent(void) {
    openrelay_spsc *r = NULL;
    int16_t filler[CAP];
    uint32_t f = 0;
    const uint32_t target = (uint32_t)(0u - 480u);

    for (uint32_t i = 0; i < CAP; i++) {
        filler[i] = (int16_t)(-6000 + (int)i);
    }
    if (openrelay_spsc_create(&r) != OPENRELAY_CAPTURE_OK) {
        bad("s4 create", 1, 0);
        return;
    }
    if (fast_forward(r, target, filler) != 0) {
        bad("s4 fast_forward", 1, 0);
        goto done;
    }
    CK(openrelay_spsc_free(r, &f) == 0 && f == CAP, "s4 empty", f, CAP);

    openrelay_spsc_push_result p;
    /* fill to 480, check, top up to 960 across the counter wrap */
    if (openrelay_spsc_push(r, filler, 480u, &p) != 0 || p.written != 480u) {
        bad("s4 push 480", p.written, 480);
        goto done;
    }
    CK(openrelay_spsc_free(r, &f) == 0 && f == CAP - 480u, "s4 at 480", f, CAP - 480u);
    if (openrelay_spsc_push(r, filler, 480u, &p) != 0 || p.written != 480u) {
        bad("s4 push cross", p.written, 480);
        goto done;
    }
    CK(openrelay_spsc_free(r, &f) == 0 && f == 0u, "s4 full after wrap", f, 0u);
    /* a push onto a full ring after the wrap must drop everything */
    if (openrelay_spsc_push(r, filler, 32u, &p) != 0 || p.written != 0u ||
        p.dropped != 32u) {
        bad("s4 drop when full", p.written, 0);
        goto done;
    }
done:
    (void)openrelay_spsc_destroy(&r);
}

/* S5: many laps of the physical index with the counters still low.
 * Positive control: the counters never approach 2^32 here, so this case says
 * nothing about the wrap and passes on the pre-fix core. */
static void s5_physical_laps(void) {
    openrelay_spsc *r = NULL;
    int16_t src[CAP], dst[MAXREAD];
    uint32_t rd = 0, expect = 0;

    if (openrelay_spsc_create(&r) != OPENRELAY_CAPTURE_OK) {
        bad("s5 create", 1, 0);
        return;
    }
    for (uint32_t lap = 0; lap < 64u; lap++) {
        uint32_t n = 240u + (lap % 4u) * 240u; /* 240..960, uneven strides */
        for (uint32_t i = 0; i < n; i++) {
            src[i] = (int16_t)((expect + i) & 0x7FFFu);
        }
        openrelay_spsc_push_result p;
        if (openrelay_spsc_push(r, src, n, &p) != 0 || p.written != n) {
            bad("s5 push", p.written, n);
            goto done;
        }
        uint32_t got = 0;
        while (got < n) {
            rd = 0;
            if (openrelay_spsc_read(r, dst, MAXREAD, &rd) != 0) {
                bad("s5 read rc", 1, 0);
                goto done;
            }
            if (rd == 0u) {
                bad("s5 premature empty", got, n);
                goto done;
            }
            for (uint32_t i = 0; i < rd; i++) {
                if (dst[i] != (int16_t)((expect + got + i) & 0x7FFFu)) {
                    bad("s5 fifo mismatch at", expect + got + i, expect + got + i);
                    goto done;
                }
            }
            got += rd;
        }
        expect += n;
    }
done:
    (void)openrelay_spsc_destroy(&r);
}

int main(void) {
    static void (*const cases[])(void) = {
        s1_read_across_wrap, s2_drain_across_wrap, s3_interleaved,
        s4_free_coherent,    s5_physical_laps,
    };
    static const char *const names[] = {
        "s1_read_across_wrap", "s2_drain_across_wrap", "s3_interleaved",
        "s4_free_coherent",    "s5_physical_laps",
    };
    unsigned long total_fail = 0;

    printf("counter-wrap scenarios (capacity=%u, 2^32 %% %u = %u)\n", CAP, CAP,
           (unsigned)((1ull << 32) % CAP));
    for (unsigned i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
        unsigned long before = g_fail;
        cases[i]();
        if (g_fail != before) {
            total_fail += g_fail - before;
            printf("[FAIL] %s\n", names[i]);
        } else {
            printf("[PASS] %s\n", names[i]);
        }
        fflush(stdout);
    }
    printf("scenarios: cases=%u failures=%lu\n",
           (unsigned)(sizeof(cases) / sizeof(cases[0])), total_fail);
    return total_fail ? 1 : 0;
}
