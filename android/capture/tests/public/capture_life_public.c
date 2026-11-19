/* T13a S3a — public lifecycle tests for the frozen capture ABI.
 * Subset: OPEN/CLOSED plus argument/ownership negatives reachable with no
 * adapter, no hardware, no STARTED. Header: include/capture-abi.h (frozen
 * product copy, sha256 298f88c6...). Plain C11, no framework, no simulated
 * core: every call is a public openrelay_capture_* symbol, so this links against
 * the real implementation or not at all. Compile-only here.
 */
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "capture-abi.h"

static int checks;
static int failures;

#define CHECK(c)                                                          \
  do {                                                                    \
    checks++;                                                             \
    if (!(c)) {                                                           \
      failures++;                                                         \
      printf("FAIL %s:%d %s\n", __FILE__, __LINE__, #c);                  \
    }                                                                     \
  } while (0)

#define CHECK_EQ(g, w)                                                    \
  do {                                                                    \
    long long g_ = (long long)(g), w_ = (long long)(w);                   \
    checks++;                                                             \
    if (g_ != w_) {                                                       \
      failures++;                                                         \
      printf("FAIL %s:%d %s=%lld want %lld\n", __FILE__, __LINE__, #g,    \
             g_, w_);                                                     \
    }                                                                     \
  } while (0)

#define BUF_SAMPLES (OPENRELAY_CAPTURE_FRAME_SAMPLES + 8u)
#define CANARY ((int16_t)0x5A5A)

static void fill_canary(int16_t *b, size_t n) {
  size_t i;
  for (i = 0; i < n; i++) b[i] = CANARY;
}

static int canary_ok(const int16_t *b, size_t n) {
  size_t i;
  for (i = 0; i < n; i++)
    if (b[i] != CANARY) return 0;
  return 1;
}

static int same_format(const openrelay_capture_format *a, const openrelay_capture_format *b) {
  return a->sample_rate_hz == b->sample_rate_hz && a->channels == b->channels &&
         a->bytes_per_sample == b->bytes_per_sample &&
         a->frame_samples == b->frame_samples && a->reserved0 == b->reserved0;
}

/* Normative fields only; padding is never read. */
static int same_status(const openrelay_capture_status *a,
                       const openrelay_capture_status *b) {
  return a->generation == b->generation && a->state == b->state &&
         a->last_error == b->last_error &&
         same_format(&a->requested, &b->requested) &&
         same_format(&a->effective, &b->effective);
}

static openrelay_capture *open_ring(void) {
  openrelay_capture *cap = NULL;
  CHECK_EQ(openrelay_capture_open(&cap, OPENRELAY_CAPTURE_RING_SAMPLES), OPENRELAY_CAPTURE_OK);
  CHECK(cap != NULL);
  return cap;
}

/* open: out null is ERR_NULL; a capacity other than the ring is ERR_ARGS and
 * leaves *out null. Initial generation/format are not fixed by the contract. */
static void test_open_arguments(void) {
  openrelay_capture *cap;
  CHECK_EQ(openrelay_capture_open(NULL, OPENRELAY_CAPTURE_RING_SAMPLES),
           OPENRELAY_CAPTURE_ERR_NULL);
  cap = (openrelay_capture *)(uintptr_t)0x1;
  CHECK_EQ(openrelay_capture_open(&cap, OPENRELAY_CAPTURE_RING_SAMPLES - 1u),
           OPENRELAY_CAPTURE_ERR_ARGS);
  CHECK(cap == NULL);
  cap = (openrelay_capture *)(uintptr_t)0x1;
  CHECK_EQ(openrelay_capture_open(&cap, OPENRELAY_CAPTURE_RING_SAMPLES + 1u),
           OPENRELAY_CAPTURE_ERR_ARGS);
  CHECK(cap == NULL);
}

/* open(960) yields OPEN; close yields CLOSED keeping generation; close
 * repeated is OK and does not free; start on CLOSED is ERR_CLOSED, no change. */
static void test_open_close_start(void) {
  openrelay_capture *cap = open_ring();
  openrelay_capture_status st, before;
  uint64_t gen_open, gen_closed;
  if (!cap) return;
  memset(&st, 0xFF, sizeof st);
  CHECK_EQ(openrelay_capture_get_status(cap, &st), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(st.state, OPENRELAY_CAPTURE_STATE_OPEN);
  gen_open = st.generation; /* value not fixed, only carried across close */
  CHECK_EQ(openrelay_capture_close(cap), OPENRELAY_CAPTURE_OK);
  memset(&st, 0xFF, sizeof st);
  CHECK_EQ(openrelay_capture_get_status(cap, &st), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(st.state, OPENRELAY_CAPTURE_STATE_CLOSED);
  CHECK(st.generation == gen_open);
  gen_closed = st.generation;
  before = st;
  CHECK_EQ(openrelay_capture_close(cap), OPENRELAY_CAPTURE_OK); /* still alive */
  memset(&st, 0xFF, sizeof st);
  CHECK_EQ(openrelay_capture_get_status(cap, &st), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(st.state, OPENRELAY_CAPTURE_STATE_CLOSED);
  CHECK(st.generation == gen_closed);
  CHECK(same_status(&st, &before));
  CHECK_EQ(openrelay_capture_start(cap), OPENRELAY_CAPTURE_ERR_CLOSED);
  memset(&st, 0xFF, sizeof st);
  CHECK_EQ(openrelay_capture_get_status(cap, &st), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(st.state, OPENRELAY_CAPTURE_STATE_CLOSED);
  CHECK(st.generation == gen_closed);
  CHECK(same_status(&st, &before));
  CHECK_EQ(openrelay_capture_destroy(&cap), OPENRELAY_CAPTURE_OK);
  CHECK(cap == NULL);
}

/* get_status: out null is ERR_NO_OUTPUT; cap null is ERR_NULL with the
 * destination struct untouched. */
static void test_get_status_arguments(void) {
  openrelay_capture *cap = open_ring();
  openrelay_capture_status st, before;
  if (!cap) return;
  CHECK_EQ(openrelay_capture_get_status(cap, NULL), OPENRELAY_CAPTURE_ERR_NO_OUTPUT);
  memset(&st, 0xA5, sizeof st);
  before = st;
  CHECK_EQ(openrelay_capture_get_status(NULL, &st), OPENRELAY_CAPTURE_ERR_NULL);
  CHECK(same_status(&st, &before));
  CHECK_EQ(openrelay_capture_close(cap), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(openrelay_capture_destroy(&cap), OPENRELAY_CAPTURE_OK);
  CHECK(cap == NULL);
}

/* read on an empty OPEN and an empty CLOSED stream: 0 samples, no silence
 * fill, canaries intact. Top 0 accepts dst null; top > 240 is ERR_ARGS; dst
 * null with positive top is ERR_NULL; n null is ERR_NO_OUTPUT; cap null is
 * ERR_NULL. On any error *n is 0. One consumer, no producers. */
static void test_read_empty(void) {
  openrelay_capture *cap = open_ring();
  int16_t buf[BUF_SAMPLES];
  uint32_t n;
  if (!cap) return;
  fill_canary(buf, BUF_SAMPLES);
  n = OPENRELAY_CAPTURE_FRAME_SAMPLES;
  CHECK_EQ(openrelay_capture_read(cap, buf, &n), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(n, 0);
  CHECK(canary_ok(buf, BUF_SAMPLES));
  n = 0;
  CHECK_EQ(openrelay_capture_read(cap, NULL, &n), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(n, 0);
  fill_canary(buf, BUF_SAMPLES);
  n = OPENRELAY_CAPTURE_FRAME_SAMPLES + 1u;
  CHECK_EQ(openrelay_capture_read(cap, buf, &n), OPENRELAY_CAPTURE_ERR_ARGS);
  CHECK_EQ(n, 0);
  CHECK(canary_ok(buf, BUF_SAMPLES));
  CHECK_EQ(openrelay_capture_read(cap, buf, NULL), OPENRELAY_CAPTURE_ERR_NO_OUTPUT);
  n = 1;
  CHECK_EQ(openrelay_capture_read(cap, NULL, &n), OPENRELAY_CAPTURE_ERR_NULL);
  CHECK_EQ(n, 0);
  n = 1;
  CHECK_EQ(openrelay_capture_read(NULL, buf, &n), OPENRELAY_CAPTURE_ERR_NULL);
  CHECK_EQ(n, 0);
  CHECK_EQ(openrelay_capture_close(cap), OPENRELAY_CAPTURE_OK);
  fill_canary(buf, BUF_SAMPLES);
  n = OPENRELAY_CAPTURE_FRAME_SAMPLES;
  CHECK_EQ(openrelay_capture_read(cap, buf, &n), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(n, 0);
  CHECK(canary_ok(buf, BUF_SAMPLES));
  CHECK_EQ(openrelay_capture_destroy(&cap), OPENRELAY_CAPTURE_OK);
  CHECK(cap == NULL);
}

/* drain on an empty OPEN and an empty CLOSED stream: 0 dropped; dropped null
 * is ERR_NO_OUTPUT; cap null is ERR_NULL. */
static void test_drain_empty(void) {
  openrelay_capture *cap = open_ring();
  uint32_t dropped;
  if (!cap) return;
  dropped = 7;
  CHECK_EQ(openrelay_capture_drain(cap, &dropped), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(dropped, 0);
  CHECK_EQ(openrelay_capture_drain(cap, NULL), OPENRELAY_CAPTURE_ERR_NO_OUTPUT);
  dropped = 7;
  CHECK_EQ(openrelay_capture_drain(NULL, &dropped), OPENRELAY_CAPTURE_ERR_NULL);
  CHECK_EQ(dropped, 0);
  CHECK_EQ(openrelay_capture_close(cap), OPENRELAY_CAPTURE_OK);
  dropped = 7;
  CHECK_EQ(openrelay_capture_drain(cap, &dropped), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(dropped, 0);
  CHECK_EQ(openrelay_capture_destroy(&cap), OPENRELAY_CAPTURE_OK);
  CHECK(cap == NULL);
}

/* push with a readable buffer on OPEN and on CLOSED is ERR_CLOSED by state:
 * no data, no state change. Pass the generation from the control snapshot;
 * no initial value is assumed and no permission is simulated. */
static void test_push_state(void) {
  openrelay_capture *cap = open_ring();
  int16_t samples[4] = {1, 2, 3, 4};
  openrelay_capture_status st, before;
  int16_t buf[BUF_SAMPLES];
  uint32_t n;
  if (!cap) return;
  memset(&before, 0xFF, sizeof before);
  CHECK_EQ(openrelay_capture_get_status(cap, &before), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(before.state, OPENRELAY_CAPTURE_STATE_OPEN);
  CHECK_EQ(openrelay_capture_push(cap, before.generation, samples, 4u), OPENRELAY_CAPTURE_ERR_CLOSED);
  memset(&st, 0xFF, sizeof st);
  CHECK_EQ(openrelay_capture_get_status(cap, &st), OPENRELAY_CAPTURE_OK);
  CHECK(same_status(&st, &before));
  fill_canary(buf, BUF_SAMPLES);
  n = OPENRELAY_CAPTURE_FRAME_SAMPLES;
  CHECK_EQ(openrelay_capture_read(cap, buf, &n), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(n, 0);
  CHECK(canary_ok(buf, BUF_SAMPLES));
  CHECK_EQ(openrelay_capture_close(cap), OPENRELAY_CAPTURE_OK);
  memset(&before, 0xFF, sizeof before);
  CHECK_EQ(openrelay_capture_get_status(cap, &before), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(openrelay_capture_push(cap, before.generation, samples, 4u), OPENRELAY_CAPTURE_ERR_CLOSED);
  memset(&st, 0xFF, sizeof st);
  CHECK_EQ(openrelay_capture_get_status(cap, &st), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(st.state, OPENRELAY_CAPTURE_STATE_CLOSED);
  CHECK(same_status(&st, &before));
  fill_canary(buf, BUF_SAMPLES);
  n = OPENRELAY_CAPTURE_FRAME_SAMPLES;
  CHECK_EQ(openrelay_capture_read(cap, buf, &n), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(n, 0);
  CHECK(canary_ok(buf, BUF_SAMPLES));
  CHECK_EQ(openrelay_capture_destroy(&cap), OPENRELAY_CAPTURE_OK);
  CHECK(cap == NULL);
}

/* destroy: owner null and *owner null are ERR_NULL; on OPEN it is ERR_STATE
 * with the pointer unchanged; after close it is OK and writes null. No stale
 * alias is ever passed: the ABI has no handle table. */
static void test_destroy_ownership(void) {
  openrelay_capture *cap = NULL;
  openrelay_capture *alias;
  CHECK_EQ(openrelay_capture_destroy(NULL), OPENRELAY_CAPTURE_ERR_NULL);
  CHECK_EQ(openrelay_capture_destroy(&cap), OPENRELAY_CAPTURE_ERR_NULL);
  CHECK_EQ(openrelay_capture_open(&cap, OPENRELAY_CAPTURE_RING_SAMPLES), OPENRELAY_CAPTURE_OK);
  if (!cap) return;
  alias = cap;
  CHECK_EQ(openrelay_capture_destroy(&cap), OPENRELAY_CAPTURE_ERR_STATE);
  CHECK(cap == alias);
  CHECK_EQ(openrelay_capture_close(cap), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(openrelay_capture_destroy(&cap), OPENRELAY_CAPTURE_OK);
  CHECK(cap == NULL);
  /* alias is not evaluated after release. */
}

int main(void) {
  test_open_arguments();
  test_open_close_start();
  test_get_status_arguments();
  test_read_empty();
  test_drain_empty();
  test_push_state();
  test_destroy_ownership();
  printf("%d checks, %d failures\n", checks, failures);
  return failures == 0 ? 0 : 1;
}
