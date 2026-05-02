cmake_minimum_required(VERSION 3.14)
project(gsm-sip-bridge VERSION 1.0.0 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_EXPORT_COMPILE_COMMANDS ON)

find_package(PkgConfig REQUIRED)
pkg_check_modules(ALSA REQUIRED alsa)

include(FetchContent)
FetchContent_Declare(
    prometheus-cpp
    GIT_REPOSITORY https://github.com/jupp0r/prometheus-cpp.git
    GIT_TAG v1.3.0
)
set(ENABLE_PUSH OFF CACHE BOOL "" FORCE)
set(ENABLE_COMPRESSION OFF CACHE BOOL "" FORCE)
set(ENABLE_TESTING OFF CACHE BOOL "Disable prometheus-cpp tests" FORCE)
set(GENERATE_PKGCONFIG OFF CACHE BOOL "" FORCE)
FetchContent_MakeAvailable(prometheus-cpp)

add_executable(audio-echo
    src/main.cpp
    src/device_discovery.cpp
    src/serial_port.cpp
    src/at_commander.cpp
    src/audio_loop.cpp
)

target_include_directories(audio-echo PRIVATE
    src
    ${ALSA_INCLUDE_DIRS}
)

target_link_libraries(audio-echo PRIVATE
    ${ALSA_LIBRARIES}
    pthread
)

target_compile_options(audio-echo PRIVATE
    -Wall -Wextra -Wpedantic -Werror
)

# --- SIP Echo binary ---
pkg_check_modules(PJSIP REQUIRED libpjproject)

add_executable(sip-echo
    src/sip/main.cpp
    src/sip/sip_config.cpp
    src/sip/echo_account.cpp
    src/sip/echo_call.cpp
)

target_include_directories(sip-echo PRIVATE
    src
    vendor
    ${PJSIP_INCLUDE_DIRS}
)

target_link_libraries(sip-echo PRIVATE
    ${PJSIP_LIBRARIES}
    pthread
)

target_link_directories(sip-echo PRIVATE
    ${PJSIP_LIBRARY_DIRS}
)

target_compile_options(sip-echo PRIVATE
    -Wall -Wextra -Wpedantic -Werror
    ${PJSIP_CFLAGS_OTHER}
)

# --- GSM-SIP Bridge binary ---
add_executable(gsm-sip-bridge
    src/bridge/main.cpp
    src/bridge/bridge_config.cpp
    src/bridge/bridge_account.cpp
    src/bridge/bridge_call.cpp
    src/bridge/card_instance.cpp
    src/bridge/card_pool.cpp
    src/bridge/alsa_media_port.cpp
    src/bridge/beep_generator.cpp
    src/bridge/metrics.cpp
    src/sip/sip_config.cpp
    src/device_discovery.cpp
    src/serial_port.cpp
    src/at_commander.cpp
)

target_include_directories(gsm-sip-bridge PRIVATE
    src
    vendor
    ${ALSA_INCLUDE_DIRS}
    ${PJSIP_INCLUDE_DIRS}
)

target_link_libraries(gsm-sip-bridge PRIVATE
    ${ALSA_LIBRARIES}
    ${PJSIP_LIBRARIES}
    prometheus-cpp::pull
    prometheus-cpp::core
    pthread
)

target_link_directories(gsm-sip-bridge PRIVATE
    ${PJSIP_LIBRARY_DIRS}
)

target_compile_options(gsm-sip-bridge PRIVATE
    -Wall -Wextra -Wpedantic -Werror
    ${PJSIP_CFLAGS_OTHER}
)

enable_testing()

find_package(GTest QUIET)
if(NOT GTest_FOUND)
    include(FetchContent)
    FetchContent_Declare(
        googletest
        GIT_REPOSITORY https://github.com/google/googletest.git
        GIT_TAG v1.15.2
    )
    set(gtest_force_shared_crt ON CACHE BOOL "" FORCE)
    FetchContent_MakeAvailable(googletest)
    set(GTest_FOUND TRUE)
endif()

if(GTest_FOUND)
    add_executable(audio-echo-tests
        tests/integration/test_device_discovery.cpp
        tests/integration/test_serial_port.cpp
        tests/integration/test_at_commander.cpp
        tests/integration/test_audio_loop.cpp
        tests/integration/test_end_to_end.cpp
        src/device_discovery.cpp
        src/serial_port.cpp
        src/at_commander.cpp
        src/audio_loop.cpp
    )

    target_include_directories(audio-echo-tests PRIVATE
        src
        tests/integration
        ${ALSA_INCLUDE_DIRS}
    )

    target_link_libraries(audio-echo-tests PRIVATE
        GTest::gtest GTest::gtest_main
        ${ALSA_LIBRARIES}
        pthread
        util
    )

    target_compile_options(audio-echo-tests PRIVATE
        -Wall -Wextra -Wpedantic
    )

    add_executable(sip-echo-tests
        tests/integration/test_sip_config.cpp
        tests/integration/test_sip_echo.cpp
        src/sip/sip_config.cpp
        src/sip/echo_account.cpp
        src/sip/echo_call.cpp
    )

    target_include_directories(sip-echo-tests PRIVATE
        src
        vendor
        tests/integration
        ${PJSIP_INCLUDE_DIRS}
    )

    target_link_libraries(sip-echo-tests PRIVATE
        GTest::gtest GTest::gtest_main
        ${PJSIP_LIBRARIES}
        pthread
    )

    target_link_directories(sip-echo-tests PRIVATE
        ${PJSIP_LIBRARY_DIRS}
    )

    target_compile_options(sip-echo-tests PRIVATE
        -Wall -Wextra -Wpedantic
        ${PJSIP_CFLAGS_OTHER}
    )

    add_executable(bridge-tests
        tests/integration/test_bridge_config.cpp
        tests/integration/test_ring_buffer.cpp
        tests/integration/test_beep_generator.cpp
        tests/integration/test_bridge_call.cpp
        tests/integration/test_card_pool.cpp
        tests/integration/test_metrics.cpp
        src/bridge/bridge_config.cpp
        src/bridge/bridge_account.cpp
        src/bridge/bridge_call.cpp
        src/bridge/card_instance.cpp
        src/bridge/card_pool.cpp
        src/bridge/alsa_media_port.cpp
        src/bridge/beep_generator.cpp
        src/bridge/metrics.cpp
        src/sip/sip_config.cpp
        src/device_discovery.cpp
        src/serial_port.cpp
        src/at_commander.cpp
    )

    target_include_directories(bridge-tests PRIVATE
        src
        vendor
        tests/integration
        ${PJSIP_INCLUDE_DIRS}
        ${ALSA_INCLUDE_DIRS}
    )

    target_link_libraries(bridge-tests PRIVATE
        GTest::gtest GTest::gtest_main
        ${PJSIP_LIBRARIES}
        ${ALSA_LIBRARIES}
        prometheus-cpp::pull
        prometheus-cpp::core
        pthread
    )

    target_link_directories(bridge-tests PRIVATE
        ${PJSIP_LIBRARY_DIRS}
    )

    target_compile_options(bridge-tests PRIVATE
        -Wall -Wextra -Wpedantic
        ${PJSIP_CFLAGS_OTHER}
    )

    include(GoogleTest)
    gtest_discover_tests(audio-echo-tests)
    gtest_discover_tests(sip-echo-tests)
    gtest_discover_tests(bridge-tests)
else()
    message(WARNING "GTest not found, tests will not be built")
endif()
