/* openrelay capture - adapter Oboe del backend privado (T13a).
 * Implementa openrelay_be_ops sobre oboe::AudioStream de entrada y la factory
 * openrelay_capture_backend_create de capture-backend.h. Un solo stream por objeto.
 * El callback de datos empuja con openrelay_capture_push; el hilo de error llama a
 * openrelay_capture_note_disconnect / note_error con el token de la generacion.
 * Retiro: contadores atomicos de callbacks dentro (DATA/ERROR); mientras el
 * stream no este cerrado se reporta FW|MAY_REENTER. */
#include <atomic>
#include <cstdint>
#include <memory>
#include <new>

#include <oboe/Oboe.h>

#include "capture-backend.h"

namespace {

struct openrelay_oboe;

class OboeCb final : public oboe::AudioStreamDataCallback,
                     public oboe::AudioStreamErrorCallback {
 public:
  explicit OboeCb(openrelay_oboe *self) : self_(self) {}
  oboe::DataCallbackResult onAudioReady(oboe::AudioStream *stream,
                                        void *data, int32_t frames) override;
  void onErrorAfterClose(oboe::AudioStream *stream,
                         oboe::Result error) override;

 private:
  openrelay_oboe *self_;
};

/* openrelay_be primero: el motor solo lee ops sobre este puntero. */
struct openrelay_oboe {
  openrelay_be be;
  openrelay_capture *owner = nullptr;
  std::shared_ptr<oboe::AudioStream> stream; /* null hasta prepare */
  std::shared_ptr<OboeCb> cb;
  std::atomic<uint64_t> gen{0};
  std::atomic<uint32_t> data_inside{0};
  std::atomic<uint32_t> err_inside{0};
  bool closed = true; /* sin stream abierto */
  bool has_epoch = false;
  uint64_t epoch = 0;
};

openrelay_oboe *as_oboe(openrelay_be *be) { return reinterpret_cast<openrelay_oboe *>(be); }

oboe::DataCallbackResult OboeCb::onAudioReady(oboe::AudioStream *stream, void *data,
                                              int32_t frames) {
  openrelay_oboe *o = self_;
  o->data_inside.fetch_add(1u);
  /* n del contrato son muestras: frames por canales (CHANNELS=1 hoy). */
  uint32_t n = static_cast<uint32_t>(frames) *
               static_cast<uint32_t>(stream->getChannelCount());
  openrelay_capture_push(o->owner, o->gen.load(),
                   static_cast<const int16_t *>(data), n);
  o->data_inside.fetch_sub(1u);
  return oboe::DataCallbackResult::Continue;
}

void OboeCb::onErrorAfterClose(oboe::AudioStream *, oboe::Result error) {
  openrelay_oboe *o = self_;
  uint64_t g = o->gen.load();
  o->err_inside.fetch_add(1u);
  if (error == oboe::Result::ErrorDisconnected) {
    openrelay_capture_note_disconnect(o->owner, g);
  } else {
    openrelay_capture_note_error(o->owner, g, static_cast<int32_t>(error));
  }
  o->err_inside.fetch_sub(1u);
}

/* Idempotente. Conserva el objeto de stream para poder releerlo. */
void close_stream(openrelay_oboe *o) {
  if (o->stream && !o->closed) {
    o->stream->close();
  }
  o->closed = true;
}

/* Relee el formato en vigor del stream abierto. frame_samples no es dato de
 * Oboe: es la constante del cable. */
void fill_format(const oboe::AudioStream *s, openrelay_capture_format *out) {
  out->sample_rate_hz = static_cast<uint32_t>(s->getSampleRate());
  out->channels = static_cast<uint16_t>(s->getChannelCount());
  out->bytes_per_sample = static_cast<uint16_t>(s->getBytesPerSample());
  out->frame_samples = OPENRELAY_CAPTURE_FRAME_SAMPLES;
  out->reserved0 = 0u;
}

int32_t op_prepare(openrelay_be *be, const openrelay_capture_format *requested,
                   openrelay_capture_format *effective) {
  openrelay_oboe *o = as_oboe(be);
  if (requested == nullptr || effective == nullptr) return OPENRELAY_CAPTURE_ERR_NULL;
  close_stream(o); /* prepare reabre desde cero */

  oboe::AudioStreamBuilder b;
  b.setDirection(oboe::Direction::Input)
      ->setSharingMode(oboe::SharingMode::Shared)
      ->setPerformanceMode(oboe::PerformanceMode::LowLatency)
      ->setFormat(oboe::AudioFormat::I16)
      ->setChannelCount(static_cast<int32_t>(requested->channels))
      ->setSampleRate(static_cast<int32_t>(requested->sample_rate_hz))
      ->setDataCallback(o->cb)
      ->setErrorCallback(o->cb);

  std::shared_ptr<oboe::AudioStream> s;
  if (b.openStream(s) != oboe::Result::OK || !s) return OPENRELAY_CAPTURE_ERR_STATE;
  o->stream = s;
  o->closed = false;
  fill_format(s.get(), effective);
  return OPENRELAY_CAPTURE_OK;
}

int32_t op_effective(openrelay_be *be, openrelay_capture_format *out) {
  openrelay_oboe *o = as_oboe(be);
  if (out == nullptr) return OPENRELAY_CAPTURE_ERR_NO_OUTPUT;
  if (!o->stream || o->closed) return OPENRELAY_CAPTURE_ERR_STATE;
  fill_format(o->stream.get(), out);
  return OPENRELAY_CAPTURE_OK;
}

int32_t op_enable(openrelay_be *be, uint64_t generation) {
  openrelay_oboe *o = as_oboe(be);
  if (!o->stream || o->closed) return OPENRELAY_CAPTURE_ERR_STATE;
  /* Un token nuevo no pisa callbacks del anterior todavia dentro. */
  if (o->has_epoch && generation != o->epoch &&
      (o->data_inside.load() != 0u || o->err_inside.load() != 0u)) {
    return OPENRELAY_CAPTURE_ERR_BUSY;
  }
  o->gen.store(generation);
  o->epoch = generation;
  o->has_epoch = true;
  if (o->stream->requestStart() != oboe::Result::OK) return OPENRELAY_CAPTURE_ERR_STATE;
  return OPENRELAY_CAPTURE_OK;
}

int32_t op_disable(openrelay_be *be) {
  openrelay_oboe *o = as_oboe(be);
  if (!o->stream || o->closed) return OPENRELAY_CAPTURE_OK;
  if (o->stream->requestStop() != oboe::Result::OK) return OPENRELAY_CAPTURE_ERR_STATE;
  return OPENRELAY_CAPTURE_OK;
}

int32_t op_close(openrelay_be *be) {
  close_stream(as_oboe(be));
  return OPENRELAY_CAPTURE_OK;
}

int32_t op_retire(openrelay_be *be, uint64_t generation, uint32_t *flags) {
  openrelay_oboe *o = as_oboe(be);
  if (flags == nullptr) return OPENRELAY_CAPTURE_ERR_NO_OUTPUT;
  uint32_t data = o->data_inside.load();
  uint32_t err = o->err_inside.load();
  if (!o->has_epoch) {
    /* Nunca habilitada: solo acredita retiro si ya cerrada y sin callbacks. */
    if (o->closed && data == 0u && err == 0u) {
      *flags = 0u;
      return OPENRELAY_CAPTURE_OK;
    }
    *flags = OPENRELAY_BE_RETIRE_ALL;
    return OPENRELAY_CAPTURE_ERR_ARGS;
  }
  if (generation != o->epoch) {
    *flags = OPENRELAY_BE_RETIRE_ALL;
    return OPENRELAY_CAPTURE_ERR_ARGS;
  }
  uint32_t f = 0u;
  if (data != 0u) f |= OPENRELAY_BE_RETIRE_DATA;
  if (err != 0u) f |= OPENRELAY_BE_RETIRE_ERROR;
  if (!o->closed) f |= OPENRELAY_BE_RETIRE_FW | OPENRELAY_BE_RETIRE_MAY_REENTER;
  *flags = f;
  return OPENRELAY_CAPTURE_OK;
}

void op_release(openrelay_be *be) {
  openrelay_oboe *o = as_oboe(be);
  close_stream(o);
  o->stream.reset();
  delete o;
}

const openrelay_be_ops g_ops = {op_prepare, op_effective, op_enable, op_disable,
                          op_close,   op_retire,    op_release};

}  // namespace

int32_t openrelay_capture_backend_create(openrelay_be **out, openrelay_capture *owner) {
  if (out == nullptr) return OPENRELAY_CAPTURE_ERR_NULL;
  *out = nullptr;
  if (owner == nullptr) return OPENRELAY_CAPTURE_ERR_NULL;
  openrelay_oboe *o = new (std::nothrow) openrelay_oboe();
  if (o == nullptr) return OPENRELAY_CAPTURE_ERR_STATE;
  o->be.ops = &g_ops;
  o->owner = owner;
  o->cb.reset(new (std::nothrow) OboeCb(o));
  if (!o->cb) {
    delete o;
    return OPENRELAY_CAPTURE_ERR_STATE;
  }
  *out = &o->be;
  return OPENRELAY_CAPTURE_OK;
}
