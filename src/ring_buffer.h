#pragma once

#include <atomic>
#include <cstddef>
#include <cstring>
#include <vector>

template <typename T>
class RingBuffer {
public:
    explicit RingBuffer(size_t capacity)
        : capacity_(capacity), buffer_(capacity) {}

    bool try_write(const T* data, size_t count) {
        if (count > capacity_) return false;

        size_t w = write_pos_.load(std::memory_order_relaxed);
        size_t r = read_pos_.load(std::memory_order_acquire);

        size_t available = capacity_ - (w - r);
        if (count > available) {
            size_t overflow = count - available;
            read_pos_.store(r + overflow, std::memory_order_release);
        }

        size_t offset = w % capacity_;
        size_t first_chunk = capacity_ - offset;

        if (count <= first_chunk) {
            std::memcpy(&buffer_[offset], data, count * sizeof(T));
        } else {
            std::memcpy(&buffer_[offset], data, first_chunk * sizeof(T));
            std::memcpy(&buffer_[0], data + first_chunk, (count - first_chunk) * sizeof(T));
        }

        write_pos_.store(w + count, std::memory_order_release);
        return true;
    }

    size_t read(T* data, size_t max_count) {
        size_t r = read_pos_.load(std::memory_order_relaxed);
        size_t w = write_pos_.load(std::memory_order_acquire);

        size_t available = w - r;
        size_t to_read = (max_count < available) ? max_count : available;
        if (to_read == 0) return 0;

        size_t offset = r % capacity_;
        size_t first_chunk = capacity_ - offset;

        if (to_read <= first_chunk) {
            std::memcpy(data, &buffer_[offset], to_read * sizeof(T));
        } else {
            std::memcpy(data, &buffer_[offset], first_chunk * sizeof(T));
            std::memcpy(data + first_chunk, &buffer_[0], (to_read - first_chunk) * sizeof(T));
        }

        read_pos_.store(r + to_read, std::memory_order_release);
        return to_read;
    }

    size_t available_read() const {
        size_t w = write_pos_.load(std::memory_order_acquire);
        size_t r = read_pos_.load(std::memory_order_relaxed);
        return w - r;
    }

    size_t available_write() const {
        size_t w = write_pos_.load(std::memory_order_relaxed);
        size_t r = read_pos_.load(std::memory_order_acquire);
        return capacity_ - (w - r);
    }

    void reset() {
        write_pos_.store(0, std::memory_order_relaxed);
        read_pos_.store(0, std::memory_order_relaxed);
    }

private:
    size_t capacity_;
    std::vector<T> buffer_;
    std::atomic<size_t> write_pos_{0};
    std::atomic<size_t> read_pos_{0};
};
