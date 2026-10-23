#pragma once
#include <stddef.h>
#include <stdint.h>

#define WC_MAX_STR      256
#define WC_MAX_DEVICES   32
#define WC_MAX_PARAMS    32
#define WC_MAX_OPTIONS   32
#define WC_MAX_KIND      32
#define WC_MAX_LABEL     32

typedef struct {
    char unique_id[WC_MAX_STR];
    char name[WC_MAX_STR];
} WcDeviceInfo;

typedef struct {
    int  value;
    char label[WC_MAX_LABEL];
} WcParamOption;

typedef struct {
    char kind[WC_MAX_KIND];
    int  current;
    int  is_range;          // 1 = continuous range, 0 = discrete options
    int  min;               // valid when is_range = 1
    int  max;               // valid when is_range = 1
    int  step;              // valid when is_range = 1
    int  num_options;       // valid when is_range = 0
    WcParamOption options[WC_MAX_OPTIONS];
} WcParamDesc;

// List available video capture devices.
// Writes up to `capacity` entries into `out`. Returns the count written.
int wc_list_devices(WcDeviceInfo *out, int capacity);

// Open an AVCaptureSession for the given uniqueID.
// Returns an opaque handle, or NULL on failure.
void *wc_open_session(const char *unique_id);

// Stop and release a capture session.
void wc_close_session(void *handle);

// Copy the latest captured frame as JPEG into a heap-allocated buffer.
// The caller must release the buffer with wc_free_frame.
// Returns 0 on success, -1 on error.
int wc_capture_frame(void *handle, uint8_t **out_data, size_t *out_size);

// Capture a full-resolution still photo as JPEG into a heap-allocated buffer.
// Uses AVCapturePhotoOutput at the device's active format (set to the
// highest-resolution format at session-open time). The caller must release
// the buffer with wc_free_frame. Returns 0 on success, -1 on error.
int wc_capture_photo(void *handle, uint8_t **out_data, size_t *out_size);

// Free a buffer returned by wc_capture_frame or wc_capture_photo.
void wc_free_frame(uint8_t *data);

// Enumerate settable parameters for the connected device.
// Discrete params (is_range=0) have at least 2 options.
// Range params (is_range=1) expose min/max/step.
// Returns the number of WcParamDesc written into out[].
int wc_get_parameters(void *handle, WcParamDesc *out, int capacity);

// Outcome of wc_set_parameter. Negative values are refusals decided here; only
// WC_SET_USB_FAILED involves the camera, and it is the one case where
// *out_status carries something (the IOKit/USB IOReturn of the failed transfer).
//
// The distinction matters because the Rust side turns WC_SET_UNKNOWN_KIND into
// "unsupported parameter" and everything else into a message naming the real
// reason: a UVC write that times out or is refused by the camera used to be
// reported to the user as "unknown parameter type".
#define WC_SET_OK             0
#define WC_SET_UNKNOWN_KIND (-1)  /* no such control in the table */
#define WC_SET_NO_UVC       (-2)  /* this camera exposes no writable UVC interface */
#define WC_SET_BAD_VALUE    (-3)  /* the value cannot be encoded for this control */
#define WC_SET_USB_FAILED   (-4)  /* the USB control transfer failed; see *out_status */
#define WC_SET_FAILED       (-5)  /* refused locally: AVFoundation (video_format), or
                                     no unit implements this control, so nothing was
                                     ever sent to the camera */

// Set a parameter by kind name and integer value.
// Returns WC_SET_OK or one of the WC_SET_* codes above. `out_status` may be NULL;
// when non-NULL it is always written (0 unless the result is WC_SET_USB_FAILED).
int wc_set_parameter(void *handle, const char *kind, int value, uint32_t *out_status);
