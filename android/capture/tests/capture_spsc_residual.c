/* tests/capture_spsc_residual.c — residual-occupancy cases across the 2^32
 * counter wrap, through the public API only. Same single implementation as
 * the rest of the suite: src/spsc.c. No second core, no seeded private
 * state, no allocator wrappers, no fake RED build.
 *
 * Why this file exists: S1/S2 of capture_spsc_counter_scenarios.c do not
 * check a partial drain with surviving payload. S4 checks occupancy only,
 * and S5 stays below counter overflow. S3 already checks a crossing with
 * payload in flight, without a partial drain. This file repeats S3 and
 * adds residual-occupancy and partial-drain probes:
 *
 *   s3              read-only crossing: 200 samples pushed over 2^32,
 *                   read back 16 at a time (same shape as scenario S3)
 *   residual        push 200 + 50 with the wrap inside the first push,
 *                   then read 250 in FIFO order, occupancy survives
 *   drain_residual  push 400 over the wrap, drain a 100 prefix (occupancy
 *                   300 stays), push 80 more, read 380 in FIFO order
 *
 * On the reviewed pre-fix core these report mis_a=88 and 300 mismatches;
 * against src/spsc.c every case returns 0. The counter jump is
 * 2^32 % 960 == 256 slot offsets, so a slot index taken as counter % 960
 * lands 256 slots away from the live position exactly at the wrap.
 */
#include "capture-spsc.h"

#include <stdint.h>
#include <stdio.h>
#include <string.h>

#define CAP OPENRELAY_CAPTURE_RING_SAMPLES     /* 960 */
#define MAXREAD OPENRELAY_CAPTURE_FRAME_SAMPLES /* 240 */

/* Fast-forward the public counters to `target` cumulative pushes, draining
 * as we go so the ring is empty between blocks. Seconds, not hours. */
static int fast_forward(openrelay_spsc *r, uint32_t target, int16_t *filler) {
    uint64_t left = target;
    while (left) {
        uint32_t n = left > CAP ? CAP : (uint32_t)left, d = 0;
        openrelay_spsc_push_result p;
        if (openrelay_spsc_push(r, filler, n, &p) || p.written != n || p.dropped ||
            openrelay_spsc_drain(r, n, &d) || d != n) return 1;
        left -= n;
    }
    return 0;
}

/* Same shape as scenario S3: 100/40/60 pushed across the wrap, read back in
 * 16-sample windows. Returns 0 on pass. */
static int run_s3(void) {
    openrelay_spsc *r = NULL;
    int16_t filler[CAP], src[CAP], dst[MAXREAD];
    uint32_t rd = 0, expect = 0;
    const uint32_t target = (uint32_t)(0u - 100u);
    for (uint32_t i = 0; i < CAP; i++) filler[i] = (int16_t)(-6000 + (int)i);
    if (openrelay_spsc_create(&r)) return 2;
    if (fast_forward(r, target, filler)) { openrelay_spsc_destroy(&r); return 3; }
    for (uint32_t i = 0; i < CAP; i++) src[i] = (int16_t)(i & 0x7FFFu);
    openrelay_spsc_push_result p;
    const uint32_t chunks[3] = {100u, 40u, 60u};
    uint32_t off = 0;
    for (int c = 0; c < 3; c++) {
        uint32_t n = chunks[c];
        if (openrelay_spsc_push(r, src + off, n, &p) || p.written != n) {
            printf("  s3 push c=%d written=%u\n", c, p.written);
            openrelay_spsc_destroy(&r); return 4;
        }
        off += n;
    }
    int mism = 0, first = -1;
    while (expect < off) {
        uint32_t want = off - expect; if (want > 16u) want = 16u;
        rd = 0;
        if (openrelay_spsc_read(r, dst, want, &rd) || rd != want) {
            printf("  s3 read expect=%u rd=%u\n", expect, rd);
            openrelay_spsc_destroy(&r); return 5;
        }
        if (memcmp(dst, src + expect, rd * sizeof(int16_t)) != 0) {
            if (first < 0) first = (int)expect;
            mism++;
        }
        expect += rd;
    }
    printf("  s3 mismatches_windows=%d first_window=%d total=%u\n", mism, first, expect);
    openrelay_spsc_destroy(&r);
    return mism ? 1 : 0;
}

/* Residual: leave occupancy across the counter wrap, then push again.
 * FIFO of the second push is what physical indices must preserve. */
static int run_residual(void) {
    openrelay_spsc *r = NULL;
    int16_t filler[CAP], a[200], b[50], dst[16];
    const uint32_t target = (uint32_t)(0u - 100u);
    for (uint32_t i = 0; i < CAP; i++) filler[i] = (int16_t)(-6000 + (int)i);
    for (uint32_t i = 0; i < 200; i++) a[i] = (int16_t)(1000 + (int)i);
    for (uint32_t i = 0; i < 50; i++) b[i] = (int16_t)(7000 + (int)i);
    if (openrelay_spsc_create(&r)) return 2;
    if (fast_forward(r, target, filler)) { openrelay_spsc_destroy(&r); return 3; }
    openrelay_spsc_push_result p;
    if (openrelay_spsc_push(r, a, 200, &p) || p.written != 200) { openrelay_spsc_destroy(&r); return 4; }
    if (openrelay_spsc_push(r, b, 50, &p) || p.written != 50) { openrelay_spsc_destroy(&r); return 5; }
    /* read 200 of a, then 50 of b */
    uint32_t got = 0; int mis_a = 0, mis_b = 0;
    while (got < 200) {
        uint32_t rd = 0, n = 200 - got; if (n > 16) n = 16;
        if (openrelay_spsc_read(r, dst, n, &rd) || rd != n) { openrelay_spsc_destroy(&r); return 6; }
        for (uint32_t i = 0; i < rd; i++) if (dst[i] != a[got + i]) mis_a++;
        got += rd;
    }
    got = 0;
    while (got < 50) {
        uint32_t rd = 0, n = 50 - got; if (n > 16) n = 16;
        if (openrelay_spsc_read(r, dst, n, &rd) || rd != n) { openrelay_spsc_destroy(&r); return 7; }
        for (uint32_t i = 0; i < rd; i++) if (dst[i] != b[got + i]) mis_b++;
        got += rd;
    }
    printf("  residual mis_a=%d mis_b=%d\n", mis_a, mis_b);
    openrelay_spsc_destroy(&r);
    return (mis_a || mis_b) ? 1 : 0;
}

/* Drain that advances ridx while leaving a payload, then one more push past
 * the wrap. */
static int run_drain_residual(void) {
    openrelay_spsc *r = NULL;
    int16_t filler[CAP], a[400], b[80], dst[16];
    const uint32_t target = (uint32_t)(0u - 50u);
    for (uint32_t i = 0; i < CAP; i++) filler[i] = (int16_t)(-6000 + (int)i);
    for (uint32_t i = 0; i < 400; i++) a[i] = (int16_t)(2000 + (int)i);
    for (uint32_t i = 0; i < 80; i++) b[i] = (int16_t)(8000 + (int)i);
    if (openrelay_spsc_create(&r)) return 2;
    if (fast_forward(r, target, filler)) { openrelay_spsc_destroy(&r); return 3; }
    openrelay_spsc_push_result p; uint32_t d = 0;
    /* push 400 crossing the wrap (target+50 = 2^32, +350 more), drain 100,
     * leave 300 of payload in the ring */
    if (openrelay_spsc_push(r, a, 400, &p) || p.written != 400) { openrelay_spsc_destroy(&r); return 4; }
    if (openrelay_spsc_drain(r, 100, &d) || d != 100) { openrelay_spsc_destroy(&r); return 5; }
    if (openrelay_spsc_push(r, b, 80, &p) || p.written != 80) { openrelay_spsc_destroy(&r); return 6; }
    /* expect a[100..399] then b[0..79] */
    int mis = 0; uint32_t got = 0; const uint32_t total = 380;
    while (got < total) {
        uint32_t rd = 0, n = total - got; if (n > 16) n = 16;
        if (openrelay_spsc_read(r, dst, n, &rd) || rd != n) { openrelay_spsc_destroy(&r); return 7; }
        for (uint32_t i = 0; i < rd; i++) {
            int16_t want = (got + i < 300) ? a[100 + got + i] : b[got + i - 300];
            if (dst[i] != want) mis++;
        }
        got += rd;
    }
    printf("  drain_residual mismatches=%d\n", mis);
    openrelay_spsc_destroy(&r);
    return mis ? 1 : 0;
}

int main(void) {
    printf("residual probe CAP=%u jump=%u\n", CAP, (unsigned)((1ull << 32) % CAP));
    int a = run_s3(); printf("s3 rc=%d\n", a);
    int b = run_residual(); printf("residual rc=%d\n", b);
    int c = run_drain_residual(); printf("drain_residual rc=%d\n", c);
    return (a || b || c) ? 1 : 0;
}
