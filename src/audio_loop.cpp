#include "audio_loop.h"
#include "logger.h"

#include <alsa/asoundlib.h>
#include <vector>

struct AudioLoop::impl {
    snd_pcm_t* capture  = nullptr;
    snd_pcm_t* playback = nullptr;
};

static bool configure_pcm(snd_pcm_t* pcm, AudioConfig& config, const char* label) {
    snd_pcm_hw_params_t* hw_params;
    snd_pcm_hw_params_alloca(&hw_params);
    snd_pcm_hw_params_any(pcm, hw_params);

    int err;
    if ((err = snd_pcm_hw_params_set_access(pcm, hw_params,
            SND_PCM_ACCESS_RW_INTERLEAVED)) < 0) {
        LOG_ERROR("%s: set access failed: %s", label, snd_strerror(err));
        return false;
    }

    if ((err = snd_pcm_hw_params_set_format(pcm, hw_params,
            SND_PCM_FORMAT_S16_LE)) < 0) {
        LOG_ERROR("%s: set format failed: %s", label, snd_strerror(err));
        return false;
    }

    if ((err = snd_pcm_hw_params_set_channels(pcm, hw_params,
            config.channels)) < 0) {
        LOG_ERROR("%s: set channels failed: %s", label, snd_strerror(err));
        return false;
    }

    unsigned int rate = config.sample_rate;
    if ((err = snd_pcm_hw_params_set_rate_near(pcm, hw_params,
            &rate, nullptr)) < 0) {
        LOG_ERROR("%s: set rate failed: %s", label, snd_strerror(err));
        return false;
    }
    if (rate != config.sample_rate) {
        LOG_WARN("%s: requested rate %u, got %u", label, config.sample_rate, rate);
        config.sample_rate = rate;
    }

    snd_pcm_uframes_t period = config.period_frames;
    if ((err = snd_pcm_hw_params_set_period_size_near(pcm, hw_params,
            &period, nullptr)) < 0) {
        LOG_ERROR("%s: set period size failed: %s", label, snd_strerror(err));
        return false;
    }
    config.period_frames = static_cast<unsigned int>(period);

    snd_pcm_uframes_t buffer = config.buffer_frames;
    if ((err = snd_pcm_hw_params_set_buffer_size_near(pcm, hw_params,
            &buffer)) < 0) {
        LOG_ERROR("%s: set buffer size failed: %s", label, snd_strerror(err));
        return false;
    }
    config.buffer_frames = static_cast<unsigned int>(buffer);

    if ((err = snd_pcm_hw_params(pcm, hw_params)) < 0) {
        LOG_ERROR("%s: hw_params apply failed: %s", label, snd_strerror(err));
        return false;
    }

    return true;
}

AudioLoop::~AudioLoop() {
    close();
}

bool AudioLoop::open(const std::string& device_name) {
    close();
    impl_ = new impl{};

    int err;
    if ((err = snd_pcm_open(&impl_->capture, device_name.c_str(),
            SND_PCM_STREAM_CAPTURE, 0)) < 0) {
        LOG_ERROR("ALSA capture open failed for %s: %s",
                  device_name.c_str(), snd_strerror(err));
        close();
        return false;
    }

    if ((err = snd_pcm_open(&impl_->playback, device_name.c_str(),
            SND_PCM_STREAM_PLAYBACK, 0)) < 0) {
        LOG_ERROR("ALSA playback open failed for %s: %s",
                  device_name.c_str(), snd_strerror(err));
        close();
        return false;
    }

    AudioConfig cap_config = config_;
    AudioConfig play_config = config_;

    if (!configure_pcm(impl_->capture, cap_config, "capture") ||
        !configure_pcm(impl_->playback, play_config, "playback")) {
        close();
        return false;
    }

    config_ = cap_config;

    LOG_INFO("audio opened: %s", device_name.c_str());
    LOG_INFO("  capture:  rate=%u, period=%u, buffer=%u",
             cap_config.sample_rate, cap_config.period_frames,
             cap_config.buffer_frames);
    LOG_INFO("  playback: rate=%u, period=%u, buffer=%u",
             play_config.sample_rate, play_config.period_frames,
             play_config.buffer_frames);
    return true;
}

void AudioLoop::run(std::atomic<bool>& running) {
    if (!impl_ || !impl_->capture || !impl_->playback) return;

    const snd_pcm_uframes_t period = config_.period_frames;
    std::vector<int16_t> buffer(period * config_.channels);

    // Pre-fill the playback buffer with silence to prevent initial underrun.
    // Write enough silent periods to fill most of the buffer, giving the
    // capture side time to start delivering real audio.
    std::vector<int16_t> silence(period * config_.channels, 0);
    unsigned int prefill_periods = (config_.buffer_frames / period);
    if (prefill_periods > 1) prefill_periods -= 1;
    for (unsigned int i = 0; i < prefill_periods; ++i) {
        snd_pcm_writei(impl_->playback, silence.data(), period);
    }

    while (running.load(std::memory_order_relaxed)) {
        snd_pcm_sframes_t frames = snd_pcm_readi(
            impl_->capture, buffer.data(), period);

        if (frames < 0) {
            if (frames == -EPIPE) {
                LOG_WARN("capture overrun, recovering");
                snd_pcm_prepare(impl_->capture);
                continue;
            }
            if (frames == -EINTR) continue;
            LOG_ERROR("ALSA read error: %s", snd_strerror(static_cast<int>(frames)));
            break;
        }

        snd_pcm_sframes_t written = snd_pcm_writei(
            impl_->playback, buffer.data(),
            static_cast<snd_pcm_uframes_t>(frames));

        if (written < 0) {
            if (written == -EPIPE) {
                LOG_WARN("playback underrun, recovering");
                snd_pcm_prepare(impl_->playback);
                // Re-prefill with silence then retry the current audio data
                for (unsigned int i = 0; i + 1 < prefill_periods; ++i) {
                    snd_pcm_writei(impl_->playback, silence.data(), period);
                }
                snd_pcm_writei(impl_->playback, buffer.data(),
                               static_cast<snd_pcm_uframes_t>(frames));
                continue;
            }
            if (written == -EINTR) continue;
            LOG_ERROR("ALSA write error: %s", snd_strerror(static_cast<int>(written)));
            break;
        }
    }
}

void AudioLoop::close() {
    if (impl_) {
        if (impl_->capture) {
            snd_pcm_drop(impl_->capture);
            snd_pcm_close(impl_->capture);
        }
        if (impl_->playback) {
            snd_pcm_drop(impl_->playback);
            snd_pcm_close(impl_->playback);
        }
        delete impl_;
        impl_ = nullptr;
    }
}
