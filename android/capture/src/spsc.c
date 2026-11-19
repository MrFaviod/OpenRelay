/* src/spsc.c — real shared T13a SPSC capture core (C11).
 *
 * Fixed capacity OPENRELAY_CAPTURE_RING_SAMPLES (960) mono int16 samples, one
 * producer and one consumer. create/destroy are the only allocating
 * operations; push/read/drain/free never allocate, free, lock, log or do IO.
 *
 * Synchronisation: two monotonic uint32 cursors.
 *   head  — written by the producer (release), read by the consumer (acquire)
 *   tail  — written by the consumer (release), read by the producer (acquire)
 * Occupancy is (uint32_t)(head - tail), which stays correct across unsigned
 * wraparound because the difference is modulo 2^32 and capacity << 2^31.
 * Cursors never reset, so no ABA and no "reset while other side runs".
 *
 * Physical slot positions are NOT derived from those cursors. 2^32 is not a
 * multiple of 960 (2^32 % 960 == 256), so head % 960 and tail % 960 jump by 256
 * slots at the counter wrap even though occupancy is still exact; that wrote
 * the payload to a slot the consumer would not read next. widx and ridx are
 * independent per-side physical indices that wrap at capacity once per
 * advance and move in lockstep with head and tail by construction, so slot N
 * always holds the Nth oldest sample whatever the counter's phase. widx is
 * touched only by the producer and ridx only by the consumer, so neither
 * needs to be atomic.
 *
 * 32-bit cursors are used deliberately: on the 64-bit Android targets and on
 * this host they are lock-free. That claim is proven for this host by the
 * atomics_lockfree test; no global portability claim is made here.
 */
#include "capture-spsc.h"

#include <stdatomic.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>

#define SPSC_CAP OPENRELAY_CAPTURE_RING_SAMPLES
#define SPSC_MAX_READ OPENRELAY_CAPTURE_FRAME_SAMPLES

struct openrelay_spsc {
    _Atomic uint32_t head; /* producer-owned publish cursor */
    _Atomic uint32_t tail; /* consumer-owned consume cursor */
    uint32_t widx;         /* producer-owned physical write slot, 0..CAP-1 */
    uint32_t ridx;         /* consumer-owned physical read slot, 0..CAP-1 */
    int16_t samples[SPSC_CAP];
};

static uint32_t spsc_occ(uint32_t head, uint32_t tail) {
    return head - tail; /* unsigned wrap is defined and intended */
}

/* advance a physical index by take (take <= CAP), wrapping at CAP at most once */
static uint32_t spsc_advance(uint32_t idx, uint32_t take) {
    idx += take;
    if (idx >= SPSC_CAP) {
        idx -= SPSC_CAP;
    }
    return idx;
}

int32_t openrelay_spsc_create(openrelay_spsc **out) {
    openrelay_spsc *r;

    if (out == NULL) {
        return OPENRELAY_CAPTURE_ERR_NO_OUTPUT;
    }
    *out = NULL;
    r = (openrelay_spsc *)malloc(sizeof(*r));
    if (r == NULL) {
        return OPENRELAY_CAPTURE_ERR_STATE;
    }
    atomic_init(&r->head, 0u);
    atomic_init(&r->tail, 0u);
    r->widx = 0u;
    r->ridx = 0u;
    *out = r;
    return OPENRELAY_CAPTURE_OK;
}

int32_t openrelay_spsc_destroy(openrelay_spsc **owner) {
    openrelay_spsc *r;

    if (owner == NULL) {
        return OPENRELAY_CAPTURE_ERR_NULL;
    }
    r = *owner;
    if (r == NULL) {
        return OPENRELAY_CAPTURE_OK;
    }
    *owner = NULL;
    free(r);
    return OPENRELAY_CAPTURE_OK;
}

int32_t openrelay_spsc_push(openrelay_spsc *ring, const int16_t *samples, uint32_t n,
                      openrelay_spsc_push_result *out) {
    uint32_t head, tail, occ, free_n, take, first, idx;

    if (out == NULL) {
        return OPENRELAY_CAPTURE_ERR_NO_OUTPUT;
    }
    out->written = 0u;
    out->dropped = 0u;
    if (ring == NULL) {
        return OPENRELAY_CAPTURE_ERR_NULL;
    }
    if (n == 0u) {
        return OPENRELAY_CAPTURE_OK; /* samples may be null here */
    }
    if (samples == NULL) {
        return OPENRELAY_CAPTURE_ERR_NULL;
    }

    head = atomic_load_explicit(&ring->head, memory_order_relaxed);
    tail = atomic_load_explicit(&ring->tail, memory_order_acquire);
    occ = spsc_occ(head, tail);
    free_n = (occ >= SPSC_CAP) ? 0u : (SPSC_CAP - occ);
    take = (n < free_n) ? n : free_n;

    if (take > 0u) {
        idx = ring->widx;
        first = SPSC_CAP - idx;
        if (first > take) {
            first = take;
        }
        for (uint32_t i = 0; i < first; i++) {
            ring->samples[idx + i] = samples[i];
        }
        for (uint32_t i = first; i < take; i++) {
            ring->samples[i - first] = samples[i];
        }
        ring->widx = spsc_advance(idx, take);
        /* publish only after the payload stores are visible */
        atomic_store_explicit(&ring->head, head + take, memory_order_release);
    }

    out->written = take;
    out->dropped = n - take;
    return OPENRELAY_CAPTURE_OK;
}

int32_t openrelay_spsc_read(openrelay_spsc *ring, int16_t *dst, uint32_t tope, uint32_t *read) {
    uint32_t head, tail, avail, take, first, idx;

    if (read == NULL) {
        return OPENRELAY_CAPTURE_ERR_NO_OUTPUT;
    }
    *read = 0u;
    if (ring == NULL) {
        return OPENRELAY_CAPTURE_ERR_NULL;
    }
    if (tope > SPSC_MAX_READ) {
        return OPENRELAY_CAPTURE_ERR_ARGS; /* tope 0 is allowed, dst may be null */
    }
    if (tope == 0u) {
        return OPENRELAY_CAPTURE_OK;
    }
    if (dst == NULL) {
        return OPENRELAY_CAPTURE_ERR_NULL;
    }

    tail = atomic_load_explicit(&ring->tail, memory_order_relaxed);
    head = atomic_load_explicit(&ring->head, memory_order_acquire);
    avail = spsc_occ(head, tail);
    take = (tope < avail) ? tope : avail;

    if (take > 0u) {
        idx = ring->ridx;
        first = SPSC_CAP - idx;
        if (first > take) {
            first = take;
        }
        for (uint32_t i = 0; i < first; i++) {
            dst[i] = ring->samples[idx + i];
        }
        for (uint32_t i = first; i < take; i++) {
            dst[i] = ring->samples[i - first];
        }
        ring->ridx = spsc_advance(idx, take);
        /* release: the producer may overwrite these slots from now on */
        atomic_store_explicit(&ring->tail, tail + take, memory_order_release);
    }

    *read = take;
    return OPENRELAY_CAPTURE_OK;
}

int32_t openrelay_spsc_drain(openrelay_spsc *ring, uint32_t tope, uint32_t *dropped) {
    uint32_t head, tail, avail, take;

    if (dropped == NULL) {
        return OPENRELAY_CAPTURE_ERR_NO_OUTPUT;
    }
    *dropped = 0u;
    if (ring == NULL) {
        return OPENRELAY_CAPTURE_ERR_NULL;
    }

    tail = atomic_load_explicit(&ring->tail, memory_order_relaxed);
    head = atomic_load_explicit(&ring->head, memory_order_acquire);
    avail = spsc_occ(head, tail);
    take = (tope < avail) ? tope : avail;

    if (take > 0u) {
        ring->ridx = spsc_advance(ring->ridx, take);
        atomic_store_explicit(&ring->tail, tail + take, memory_order_release);
    }
    *dropped = take;
    return OPENRELAY_CAPTURE_OK;
}

int32_t openrelay_spsc_free(const openrelay_spsc *ring, uint32_t *out) {
    uint32_t head, tail, occ;

    if (out == NULL) {
        return OPENRELAY_CAPTURE_ERR_NO_OUTPUT;
    }
    *out = 0u;
    if (ring == NULL) {
        return OPENRELAY_CAPTURE_ERR_NULL;
    }
    /* Bounded observational read, not a transactional snapshot. */
    head = atomic_load_explicit(&ring->head, memory_order_acquire);
    tail = atomic_load_explicit(&ring->tail, memory_order_acquire);
    occ = spsc_occ(head, tail);
    *out = (occ >= SPSC_CAP) ? 0u : (SPSC_CAP - occ);
    return OPENRELAY_CAPTURE_OK;
}
