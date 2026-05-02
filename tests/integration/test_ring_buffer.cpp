#include <gtest/gtest.h>
#include "ring_buffer.h"
#include <cstdint>
#include <thread>
#include <vector>

TEST(RingBuffer, write_then_read_returns_same_data) {
    // Arrange
    RingBuffer<int16_t> rb(256);
    int16_t write_data[] = {10, 20, 30, 40, 50};

    // Act
    bool written = rb.try_write(write_data, 5);
    int16_t read_data[5] = {};
    size_t read_count = rb.read(read_data, 5);

    // Assert
    EXPECT_TRUE(written);
    EXPECT_EQ(read_count, 5u);
    for (int i = 0; i < 5; ++i) {
        EXPECT_EQ(read_data[i], write_data[i]);
    }
}

TEST(RingBuffer, write_to_full_overwrites_oldest) {
    // Arrange
    RingBuffer<int16_t> rb(4);
    int16_t data1[] = {1, 2, 3, 4};
    int16_t data2[] = {99};

    // Act
    bool first = rb.try_write(data1, 4);
    bool second = rb.try_write(data2, 1);

    int16_t out[4] = {};
    size_t count = rb.read(out, 4);

    // Assert
    EXPECT_TRUE(first);
    EXPECT_TRUE(second);
    EXPECT_EQ(count, 4u);
    EXPECT_EQ(out[0], 2);
    EXPECT_EQ(out[1], 3);
    EXPECT_EQ(out[2], 4);
    EXPECT_EQ(out[3], 99);
}

TEST(RingBuffer, write_exceeding_capacity_returns_false) {
    // Arrange
    RingBuffer<int16_t> rb(4);
    int16_t data[5] = {1, 2, 3, 4, 5};

    // Act
    bool result = rb.try_write(data, 5);

    // Assert
    EXPECT_FALSE(result);
}

TEST(RingBuffer, reset_clears_all_data) {
    // Arrange
    RingBuffer<int16_t> rb(16);
    int16_t data[] = {1, 2, 3, 4};
    rb.try_write(data, 4);

    // Act
    rb.reset();

    // Assert
    EXPECT_EQ(rb.available_read(), 0u);
    EXPECT_EQ(rb.available_write(), 16u);

    int16_t out[4] = {};
    EXPECT_EQ(rb.read(out, 4), 0u);
}

TEST(RingBuffer, read_from_empty_returns_zero) {
    // Arrange
    RingBuffer<int16_t> rb(64);
    int16_t buf[16] = {};

    // Act
    size_t count = rb.read(buf, 16);

    // Assert
    EXPECT_EQ(count, 0u);
}

TEST(RingBuffer, wraparound_works) {
    // Arrange
    RingBuffer<int16_t> rb(8);
    int16_t write1[] = {1, 2, 3, 4, 5, 6};
    int16_t read_buf[6] = {};

    // Act: fill most of the buffer, read it, then write across the wrap boundary
    rb.try_write(write1, 6);
    rb.read(read_buf, 6);

    int16_t write2[] = {10, 20, 30, 40, 50};
    bool ok = rb.try_write(write2, 5);
    int16_t result[5] = {};
    size_t count = rb.read(result, 5);

    // Assert
    EXPECT_TRUE(ok);
    EXPECT_EQ(count, 5u);
    for (int i = 0; i < 5; ++i) {
        EXPECT_EQ(result[i], write2[i]);
    }
}

TEST(RingBuffer, available_read_write_correct) {
    // Arrange
    RingBuffer<int16_t> rb(16);
    int16_t data[] = {1, 2, 3};

    // Act
    rb.try_write(data, 3);

    // Assert
    EXPECT_EQ(rb.available_read(), 3u);
    EXPECT_EQ(rb.available_write(), 13u);
}

TEST(RingBuffer, concurrent_producer_consumer) {
    // Arrange
    static constexpr size_t TOTAL_ITEMS = 100000;
    static constexpr size_t BATCH = 160;
    static constexpr size_t BUF_CAP = 4096;
    RingBuffer<int16_t> rb(BUF_CAP);

    std::atomic<bool> producer_done{false};
    std::atomic<size_t> total_consumed{0};

    // Act
    std::thread producer([&]() {
        int16_t buf[BATCH];
        size_t offset = 0;
        while (offset < TOTAL_ITEMS) {
            size_t chunk = std::min(BATCH, TOTAL_ITEMS - offset);
            for (size_t i = 0; i < chunk; ++i) {
                buf[i] = static_cast<int16_t>((offset + i) % 32768);
            }
            rb.try_write(buf, chunk);
            offset += chunk;
        }
        producer_done.store(true, std::memory_order_release);
    });

    std::thread consumer([&]() {
        int16_t buf[BATCH];
        while (!producer_done.load(std::memory_order_acquire) ||
               rb.available_read() > 0) {
            size_t got = rb.read(buf, BATCH);
            total_consumed.fetch_add(got, std::memory_order_relaxed);
            if (got == 0) std::this_thread::yield();
        }
    });

    producer.join();
    consumer.join();

    // Assert: no deadlock, consumer read some data, buffer is drained
    EXPECT_GT(total_consumed.load(), 0u);
    EXPECT_EQ(rb.available_read(), 0u);
}
