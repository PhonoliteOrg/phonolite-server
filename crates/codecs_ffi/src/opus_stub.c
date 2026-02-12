#include <stdint.h>

void* opus_encoder_create(int32_t fs, int32_t channels, int32_t application, int32_t* error) {
    (void)fs;
    (void)channels;
    (void)application;
    if (error) {
        *error = -1;
    }
    return 0;
}

void opus_encoder_destroy(void* st) {
    (void)st;
}

int32_t opus_encode(void* st, const int16_t* pcm, int32_t frame_size, unsigned char* data, int32_t max_data_bytes) {
    (void)st;
    (void)pcm;
    (void)frame_size;
    (void)data;
    (void)max_data_bytes;
    return -1;
}

int32_t opus_encoder_ctl(void* st, int32_t request, ...) {
    (void)st;
    (void)request;
    return -1;
}

void* opus_decoder_create(int32_t fs, int32_t channels, int32_t* error) {
    (void)fs;
    (void)channels;
    if (error) {
        *error = -1;
    }
    return 0;
}

void opus_decoder_destroy(void* st) {
    (void)st;
}

int32_t opus_decode(void* st, const unsigned char* data, int32_t len, int16_t* pcm, int32_t frame_size, int32_t decode_fec) {
    (void)st;
    (void)data;
    (void)len;
    (void)pcm;
    (void)frame_size;
    (void)decode_fec;
    return -1;
}
