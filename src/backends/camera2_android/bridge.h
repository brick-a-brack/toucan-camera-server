#pragma once
#include <stddef.h>
#include <stdint.h>

#define AC_MAX_STR      256
#define AC_MAX_DEVICES   16
#define AC_MAX_PARAMS    32
#define AC_MAX_OPTIONS   32
#define AC_MAX_KIND      64
#define AC_MAX_LABEL     64

typedef struct {
    char camera_id[AC_MAX_STR];
    char name[AC_MAX_STR];
} AcDeviceInfo;

typedef struct {
    int32_t value;
    char    label[AC_MAX_LABEL];
} AcParamOption;

typedef struct {
    char    kind[AC_MAX_KIND];
    int32_t current;
    int32_t is_range;
    int32_t min;
    int32_t max;
    int32_t step;
    int32_t num_options;
    AcParamOption options[AC_MAX_OPTIONS];
} AcParamDesc;

// Returns number of devices written, or -1 on error.
int ac_list_devices(AcDeviceInfo *out, int capacity);

// Opens a camera session. Returns opaque handle, or NULL on failure.
void *ac_open_session(const char *camera_id);

// Closes and frees a session.
void ac_close_session(void *handle);

// Captures the latest preview frame as packed RGB24.
// On success writes pointer and byte count; caller must call ac_free_frame.
// Also writes the pixel dimensions to *out_width / *out_height.
// Returns 0 on success, -1 on error.
int ac_capture_frame(void *handle,
                     uint8_t **out_data, size_t *out_size,
                     int32_t *out_width, int32_t *out_height);

// Captures a still photo.
// If *out_is_jpeg is set to 1 the buffer already contains a complete JPEG
// file; otherwise it is packed RGB24 (same layout as ac_capture_frame).
// Caller must call ac_free_frame to release the buffer.
// Returns 0 on success, -1 on error.
int ac_capture_photo(void *handle,
                     uint8_t **out_data, size_t *out_size,
                     int32_t *out_width, int32_t *out_height,
                     int32_t *out_is_jpeg);

// Frees a buffer returned by ac_capture_frame or ac_capture_photo.
void ac_free_frame(uint8_t *data);

// Writes current parameter descriptors. Returns count written, or -1 on error.
int ac_get_parameters(void *handle, AcParamDesc *out, int capacity);

// Sets a parameter. Returns 0 on success, -1 on error.
int ac_set_parameter(void *handle, const char *kind, int32_t value);
