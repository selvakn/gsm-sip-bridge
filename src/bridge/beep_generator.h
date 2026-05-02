#pragma once

#include <cstddef>
#include <cstdint>
#include <vector>

class BeepGenerator {
public:
    static constexpr unsigned int DEFAULT_FREQUENCY_HZ = 400;
    static constexpr unsigned int DEFAULT_ON_MS = 1000;
    static constexpr unsigned int DEFAULT_OFF_MS = 4000;
    static constexpr unsigned int DEFAULT_SAMPLE_RATE = 8000;
    static constexpr int16_t DEFAULT_AMPLITUDE = 16000;

    BeepGenerator(unsigned int frequency_hz = DEFAULT_FREQUENCY_HZ,
                  unsigned int on_ms = DEFAULT_ON_MS,
                  unsigned int off_ms = DEFAULT_OFF_MS,
                  unsigned int sample_rate = DEFAULT_SAMPLE_RATE,
                  int16_t amplitude = DEFAULT_AMPLITUDE);

    void fill_frame(int16_t* buf, size_t frame_count);
    void reset();

    unsigned int on_samples() const { return on_samples_; }
    unsigned int off_samples() const { return off_samples_; }
    unsigned int cycle_samples() const { return on_samples_ + off_samples_; }

private:
    unsigned int on_samples_;
    unsigned int off_samples_;
    std::vector<int16_t> tone_buffer_;
    size_t position_ = 0;
};
