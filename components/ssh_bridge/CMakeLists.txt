# components/ssh_bridge/CMakeLists.txt
#
# SSH-to-USB-CDC serial bridge component.
#
cmake_minimum_required(VERSION 3.16)

idf_component_register(
    SRCS         "ssh_bridge.c"
    INCLUDE_DIRS "include"
    REQUIRES     wolfssl wolfssh lwip freertos
)

# wolfSSL's settings.h uses #include "FreeRTOS.h".
# In ESP-IDF 5.x FreeRTOS.h is under freertos/ subdirectory.
target_include_directories(${COMPONENT_LIB} PRIVATE
    "${IDF_PATH}/components/freertos/FreeRTOS-Kernel/include/freertos"
    "${IDF_PATH}/components/freertos/FreeRTOS-Kernel/portable/xtensa/include/freertos"
)

target_compile_options(${COMPONENT_LIB} PRIVATE
    -Wno-unused-function
    -Wno-unused-variable
)
