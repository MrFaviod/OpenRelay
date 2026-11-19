/* arl playback — salida Oboe PCM16 48 kHz mono sobre el SPSC compartido.
 *
 * Productor: arl_playback_write (hilo Kotlin). Consumidor: callback de Oboe,
 * que llena el buffer de salida y completa con silencio si el anillo se queda
 * corto: el callback no bloquea ni espera al productor. El anillo es el mismo
 * arl_spsc de la captura (T13a); no se reimplementa ni se comparte con el
 * flujo de micrófono. Códigos de error: los ARL_CAPTURE_ERR_* del ABI (negativos).
 */
#include <atomic>
#include <cstdint>
#include <memory>
#include <new>

#include <oboe/Oboe.h>

#include "capture-spsc.h"

struct arl_playback;

namespace {

class PlaybackCb final : public oboe::AudioStreamDataCallback {
 public:
  explicit PlaybackCb(arl_playback *self) : self_(self) {}
  oboe::DataCallbackResult onAudioReady(oboe::AudioStream *stream, void *data,
                                        int32_t frames) override;

 private:
  arl_playback *self_;
};

}  // namespace

struct arl_playback {
  arl_spsc *ring = nullptr;
  std::shared_ptr<oboe::AudioStream> stream; /* null tras destroy */
  std::shared_ptr<PlaybackCb> cb;
  std::atomic<bool> running{false};
};

oboe::DataCallbackResult PlaybackCb::onAudioReady(oboe::AudioStream *, void *data,
                                                  int32_t frames) {
  /* Mono: un frame es una muestra. */
  auto *dst = static_cast<int16_t *>(data);
  uint32_t total = (frames > 0) ? static_cast<uint32_t>(frames) : 0u;
  uint32_t done = 0u;
  while (done < total) {
    uint32_t tope = total - done;
    if (tope > ARL_CAPTURE_FRAME_SAMPLES) {
      tope = ARL_CAPTURE_FRAME_SAMPLES; /* arl_spsc_read rechaza tope > 240 */
    }
    uint32_t got = 0u;
    if (arl_spsc_read(self_->ring, dst + done, tope, &got) != ARL_CAPTURE_OK) {
      break;
    }
    done += got;
    if (got < tope) {
      break; /* anillo vacío: el resto se completa con silencio */
    }
  }
  for (uint32_t i = done; i < total; i++) {
    dst[i] = 0;
  }
  return oboe::DataCallbackResult::Continue;
}

namespace {

int32_t open_output(arl_playback *p) {
  oboe::AudioStreamBuilder b;
  b.setDirection(oboe::Direction::Output)
      ->setSharingMode(oboe::SharingMode::Shared)
      ->setPerformanceMode(oboe::PerformanceMode::LowLatency)
      ->setFormat(oboe::AudioFormat::I16)
      ->setChannelCount(static_cast<int32_t>(ARL_CAPTURE_CHANNELS))
      ->setSampleRate(static_cast<int32_t>(ARL_CAPTURE_SAMPLE_RATE_HZ))
      ->setDataCallback(p->cb);

  std::shared_ptr<oboe::AudioStream> s;
  if (b.openStream(s) != oboe::Result::OK || !s) {
    return ARL_CAPTURE_ERR_STATE;
  }
  p->stream = s;
  return ARL_CAPTURE_OK;
}

}  // namespace

/* C ABI privada de este archivo y jni-playback.c; sin cabecera nueva. */
extern "C" {

int32_t arl_playback_create(arl_playback **out) {
  if (out == nullptr) return ARL_CAPTURE_ERR_NULL;
  *out = nullptr;
  auto *p = new (std::nothrow) arl_playback();
  if (p == nullptr) return ARL_CAPTURE_ERR_STATE;
  if (arl_spsc_create(&p->ring) != ARL_CAPTURE_OK) {
    delete p;
    return ARL_CAPTURE_ERR_STATE;
  }
  p->cb.reset(new (std::nothrow) PlaybackCb(p));
  if (!p->cb) {
    arl_spsc_destroy(&p->ring);
    delete p;
    return ARL_CAPTURE_ERR_STATE;
  }
  if (open_output(p) != ARL_CAPTURE_OK) {
    p->cb.reset();
    arl_spsc_destroy(&p->ring);
    delete p;
    return ARL_CAPTURE_ERR_STATE;
  }
  *out = p;
  return ARL_CAPTURE_OK;
}

int32_t arl_playback_start(arl_playback *p) {
  if (p == nullptr) return ARL_CAPTURE_ERR_NULL;
  if (p->running.load()) return ARL_CAPTURE_OK; /* idempotente */
  if (!p->stream) return ARL_CAPTURE_ERR_CLOSED;
  if (p->stream->requestStart() != oboe::Result::OK) {
    return ARL_CAPTURE_ERR_STATE;
  }
  p->running.store(true);
  return ARL_CAPTURE_OK;
}

/* Copia min(n, hueco) muestras; *written dice cuántas entraron. */
int32_t arl_playback_write(arl_playback *p, const int16_t *samples, uint32_t n,
                           uint32_t *written) {
  if (written == nullptr) return ARL_CAPTURE_ERR_NO_OUTPUT;
  *written = 0u;
  if (p == nullptr) return ARL_CAPTURE_ERR_NULL;
  if (n > 0u && samples == nullptr) return ARL_CAPTURE_ERR_NULL;
  if (!p->running.load()) return ARL_CAPTURE_ERR_CLOSED;
  arl_spsc_push_result r = {0u, 0u};
  int32_t rc = arl_spsc_push(p->ring, samples, n, &r);
  if (rc != ARL_CAPTURE_OK) return rc;
  *written = r.written;
  return ARL_CAPTURE_OK;
}

int32_t arl_playback_stop(arl_playback *p) {
  if (p == nullptr) return ARL_CAPTURE_ERR_NULL;
  if (!p->running.load()) return ARL_CAPTURE_OK; /* idempotente */
  p->running.store(false);
  if (!p->stream) return ARL_CAPTURE_OK;
  if (p->stream->requestStop() != oboe::Result::OK) {
    return ARL_CAPTURE_ERR_STATE;
  }
  return ARL_CAPTURE_OK;
}

/* close() retira el stream y el callback antes de liberar el anillo: no
 * queda consumidor sobre arl_spsc_destroy. */
int32_t arl_playback_destroy(arl_playback **owner) {
  if (owner == nullptr) return ARL_CAPTURE_ERR_NULL;
  arl_playback *p = *owner;
  if (p == nullptr) return ARL_CAPTURE_ERR_NULL;
  p->running.store(false);
  if (p->stream) {
    p->stream->close();
    p->stream.reset();
  }
  p->cb.reset();
  arl_spsc_destroy(&p->ring);
  delete p;
  *owner = nullptr;
  return ARL_CAPTURE_OK;
}

}  // extern "C"
