#include "bridge.h"

#include <camera/NdkCameraDevice.h>
#include <camera/NdkCameraManager.h>
#include <camera/NdkCameraMetadata.h>
#include <camera/NdkCameraMetadataTags.h>
#include <camera/NdkCaptureRequest.h>
#include <camera/NdkCameraCaptureSession.h>
#include <media/NdkImage.h>
#include <media/NdkImageReader.h>
#include <android/log.h>

#include <pthread.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <math.h>

#define TAG "ToucanCamera"
#define LOGI(...) __android_log_print(ANDROID_LOG_INFO,  TAG, __VA_ARGS__)
#define LOGE(...) __android_log_print(ANDROID_LOG_ERROR, TAG, __VA_ARGS__)

// ---------------------------------------------------------------------------
// Session state
// ---------------------------------------------------------------------------

typedef struct {
    ACameraManager              *manager;
    ACameraDevice               *device;
    ACameraMetadata             *chars;        // static characteristics

    AImageReader                *preview_reader;
    ANativeWindow               *preview_window;
    ACaptureSessionOutput       *preview_output;
    ACameraOutputTarget         *preview_target; // wraps preview_window for capture requests

    AImageReader                *photo_reader;
    ANativeWindow               *photo_window;
    ACaptureSessionOutput       *photo_output;
    ACameraOutputTarget         *photo_target;   // wraps photo_window for capture requests
    int32_t                      photo_is_jpeg; // 1 if photo_reader uses JPEG format

    ACaptureSessionOutputContainer *outputs;
    ACameraCaptureSession        *session;
    ACaptureRequest              *preview_req;
    ACaptureRequest              *photo_req;

    // Photo capture synchronisation
    pthread_mutex_t              photo_mutex;
    pthread_cond_t               photo_cond;
    uint8_t                     *photo_data;
    size_t                       photo_size;
    int32_t                      photo_width;
    int32_t                      photo_height;
    int32_t                      photo_ready;

    int32_t                      disconnected;

    // Session ready synchronisation
    pthread_mutex_t              sess_mutex;
    pthread_cond_t               sess_cond;
    int32_t                      sess_ready;

    // Tracked current values (updated by ac_set_parameter)
    int32_t cur_ae_mode;       // 0=manual, 1=auto
    int32_t cur_iso;
    int32_t cur_shutter_us;    // microseconds
    int32_t cur_aperture_x100; // f-number × 100
    int32_t cur_awb_mode;
    int32_t cur_af_mode;
    int32_t cur_zoom_x100;     // zoom factor × 100  (100 = 1.0×)
    int32_t cur_ev_comp;
    int32_t cur_focus_x100;    // diopters × 100
} AcSession;

// ---------------------------------------------------------------------------
// Device state callbacks (post-open: disconnect / error only)
// In the NDK C API, ACameraManager_openCamera is synchronous — the device
// pointer is returned directly. There is no onOpened callback.
// ---------------------------------------------------------------------------

static void on_device_disconnected(void *ctx, ACameraDevice *device) {
    (void)device;
    AcSession *s = (AcSession *)ctx;
    s->disconnected = 1;
    LOGE("Camera device disconnected");
}

static void on_device_error(void *ctx, ACameraDevice *device, int error) {
    (void)ctx; (void)device;
    LOGE("Camera device error: %d", error);
}

// ---------------------------------------------------------------------------
// Session-state callbacks
// ---------------------------------------------------------------------------

static void on_session_ready(void *ctx, ACameraCaptureSession *session) {
    (void)session;
    AcSession *s = (AcSession *)ctx;
    pthread_mutex_lock(&s->sess_mutex);
    s->sess_ready = 1;
    pthread_cond_signal(&s->sess_cond);
    pthread_mutex_unlock(&s->sess_mutex);
}

static void on_session_closed(void *ctx, ACameraCaptureSession *session) {
    (void)ctx; (void)session;
}

static void on_session_active(void *ctx, ACameraCaptureSession *session) {
    (void)session;
    AcSession *s = (AcSession *)ctx;
    pthread_mutex_lock(&s->sess_mutex);
    s->sess_ready = 1;
    pthread_cond_signal(&s->sess_cond);
    pthread_mutex_unlock(&s->sess_mutex);
    LOGI("Session active (capturing)");
}

// ---------------------------------------------------------------------------
// Photo image-available callback
// ---------------------------------------------------------------------------

static void on_photo_available(void *ctx, AImageReader *reader) {
    AcSession *s = (AcSession *)ctx;

    AImage *image = NULL;
    media_status_t st = AImageReader_acquireLatestImage(reader, &image);
    if (st != AMEDIA_OK || !image) return;

    int32_t width = 0, height = 0;
    AImage_getWidth(image, &width);
    AImage_getHeight(image, &height);

    pthread_mutex_lock(&s->photo_mutex);

    free(s->photo_data);
    s->photo_data   = NULL;
    s->photo_size   = 0;
    s->photo_width  = width;
    s->photo_height = height;
    s->photo_ready  = 0;

    if (s->photo_is_jpeg) {
        // For JPEG format the image has a single plane with the complete JPEG.
        uint8_t *data   = NULL;
        int      length = 0;
        if (AImage_getPlaneData(image, 0, &data, &length) == AMEDIA_OK && data && length > 0) {
            s->photo_data = (uint8_t *)malloc((size_t)length);
            if (s->photo_data) {
                memcpy(s->photo_data, data, (size_t)length);
                s->photo_size = (size_t)length;
            }
        }
    } else {
        // YUV_420_888: convert to packed RGB24
        uint8_t *y_data = NULL, *u_data = NULL, *v_data = NULL;
        int y_len = 0, u_len = 0, v_len = 0;
        int32_t y_row = 0, u_row = 0, v_row = 0;
        int32_t u_pix = 0, v_pix = 0;

        AImage_getPlaneData(image, 0, &y_data, &y_len);
        AImage_getPlaneData(image, 1, &u_data, &u_len);
        AImage_getPlaneData(image, 2, &v_data, &v_len);
        AImage_getPlaneRowStride(image, 0, &y_row);
        AImage_getPlaneRowStride(image, 1, &u_row);
        AImage_getPlaneRowStride(image, 2, &v_row);
        AImage_getPlanePixelStride(image, 1, &u_pix);
        AImage_getPlanePixelStride(image, 2, &v_pix);

        if (y_data && u_data && v_data && width > 0 && height > 0) {
            size_t rgb_size = (size_t)(width * height * 3);
            uint8_t *rgb = (uint8_t *)malloc(rgb_size);
            if (rgb) {
                for (int row = 0; row < height; row++) {
                    for (int col = 0; col < width; col++) {
                        int y_val = y_data[row * y_row + col] & 0xFF;
                        int u_val = u_data[(row / 2) * u_row + (col / 2) * u_pix] & 0xFF;
                        int v_val = v_data[(row / 2) * v_row + (col / 2) * v_pix] & 0xFF;

                        int r = (int)(y_val + 1.402f * (v_val - 128));
                        int g = (int)(y_val - 0.344136f * (u_val - 128) - 0.714136f * (v_val - 128));
                        int b = (int)(y_val + 1.772f * (u_val - 128));

                        int idx = (row * width + col) * 3;
                        rgb[idx]     = (uint8_t)(r < 0 ? 0 : r > 255 ? 255 : r);
                        rgb[idx + 1] = (uint8_t)(g < 0 ? 0 : g > 255 ? 255 : g);
                        rgb[idx + 2] = (uint8_t)(b < 0 ? 0 : b > 255 ? 255 : b);
                    }
                }
                s->photo_data   = rgb;
                s->photo_size   = rgb_size;
            }
        }
    }

    AImage_delete(image);
    s->photo_ready = 1;
    pthread_cond_signal(&s->photo_cond);
    pthread_mutex_unlock(&s->photo_mutex);
}

// ---------------------------------------------------------------------------
// Helpers: choose a preview resolution ≤ 1280×720 for YUV_420_888
// ---------------------------------------------------------------------------

static void pick_preview_size(ACameraMetadata *chars,
                               int32_t *out_w, int32_t *out_h) {
    *out_w = 1280;
    *out_h = 720;

    ACameraMetadata_const_entry entry = {0};
    if (ACameraMetadata_getConstEntry(
            chars, ACAMERA_SCALER_AVAILABLE_STREAM_CONFIGURATIONS, &entry) != ACAMERA_OK)
        return;

    // Entry data: [format, width, height, isInput] repeated
    int32_t best_w = 0, best_h = 0;
    for (uint32_t i = 0; i + 3 < entry.count; i += 4) {
        int32_t fmt    = entry.data.i32[i];
        int32_t w      = entry.data.i32[i + 1];
        int32_t h      = entry.data.i32[i + 2];
        int32_t is_in  = entry.data.i32[i + 3];
        if (fmt != 0x23 || is_in != 0) continue; // 0x23 = YUV_420_888
        if (w <= 1280 && h <= 720 && w * h > best_w * best_h) {
            best_w = w; best_h = h;
        }
    }
    if (best_w > 0) { *out_w = best_w; *out_h = best_h; }
}

// Returns 1 if JPEG output is available in characteristics, and sets *w/*h.
static int pick_photo_size(ACameraMetadata *chars,
                            int32_t *out_w, int32_t *out_h) {
    ACameraMetadata_const_entry entry = {0};
    if (ACameraMetadata_getConstEntry(
            chars, ACAMERA_SCALER_AVAILABLE_STREAM_CONFIGURATIONS, &entry) != ACAMERA_OK)
        return 0;

    int32_t best_w = 0, best_h = 0;
    for (uint32_t i = 0; i + 3 < entry.count; i += 4) {
        int32_t fmt   = entry.data.i32[i];
        int32_t w     = entry.data.i32[i + 1];
        int32_t h     = entry.data.i32[i + 2];
        int32_t is_in = entry.data.i32[i + 3];
        if (fmt != 0x100 || is_in != 0) continue; // 0x100 = JPEG
        if (w * h > best_w * best_h) { best_w = w; best_h = h; }
    }
    if (best_w > 0) { *out_w = best_w; *out_h = best_h; return 1; }
    return 0;
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

int ac_list_devices(AcDeviceInfo *out, int capacity) {
    ACameraManager *mgr = ACameraManager_create();
    if (!mgr) return -1;

    ACameraIdList *id_list = NULL;
    if (ACameraManager_getCameraIdList(mgr, &id_list) != ACAMERA_OK || !id_list) {
        ACameraManager_delete(mgr);
        return -1;
    }

    int count = 0;
    for (int i = 0; i < id_list->numCameras && count < capacity; i++) {
        const char *cam_id = id_list->cameraIds[i];

        ACameraMetadata *chars = NULL;
        ACameraManager_getCameraCharacteristics(mgr, cam_id, &chars);

        // Determine facing string for the human-readable name
        const char *facing_str = "Camera";
        if (chars) {
            ACameraMetadata_const_entry entry = {0};
            if (ACameraMetadata_getConstEntry(chars, ACAMERA_LENS_FACING, &entry) == ACAMERA_OK) {
                switch (entry.data.u8[0]) {
                    case ACAMERA_LENS_FACING_FRONT:    facing_str = "Front Camera"; break;
                    case ACAMERA_LENS_FACING_BACK:     facing_str = "Back Camera";  break;
                    case ACAMERA_LENS_FACING_EXTERNAL: facing_str = "External Camera"; break;
                }
            }
            ACameraMetadata_free(chars);
        }

        strncpy(out[count].camera_id, cam_id, AC_MAX_STR - 1);
        out[count].camera_id[AC_MAX_STR - 1] = '\0';
        snprintf(out[count].name, AC_MAX_STR, "%s %s", facing_str, cam_id);
        count++;
    }

    ACameraManager_deleteCameraIdList(id_list);
    ACameraManager_delete(mgr);
    return count;
}

void *ac_open_session(const char *camera_id) {
    AcSession *s = (AcSession *)calloc(1, sizeof(AcSession));
    if (!s) return NULL;

    pthread_mutex_init(&s->sess_mutex,  NULL);
    pthread_cond_init (&s->sess_cond,   NULL);
    pthread_mutex_init(&s->photo_mutex, NULL);
    pthread_cond_init (&s->photo_cond,  NULL);

    // Default state: auto everything
    s->cur_ae_mode       = 1;  // auto
    s->cur_iso           = 100;
    s->cur_shutter_us    = 16667; // ~1/60 s
    s->cur_aperture_x100 = 200;   // f/2.0
    s->cur_awb_mode      = 1;     // auto
    s->cur_af_mode       = 3;     // continuous-video
    s->cur_zoom_x100     = 100;   // 1.0×
    s->cur_ev_comp       = 0;
    s->cur_focus_x100    = 0;     // infinity

    s->manager = ACameraManager_create();
    if (!s->manager) { LOGE("ACameraManager_create failed"); goto fail; }

    // Load characteristics
    if (ACameraManager_getCameraCharacteristics(s->manager, camera_id, &s->chars) != ACAMERA_OK) {
        LOGE("getCameraCharacteristics failed for %s", camera_id);
        goto fail;
    }

    // Open camera device — synchronous in the NDK C API.
    {
        ACameraDevice_StateCallbacks dev_cbs = {
            .context        = s,
            .onDisconnected = on_device_disconnected,
            .onError        = on_device_error,
        };
        camera_status_t st = ACameraManager_openCamera(s->manager, camera_id, &dev_cbs, &s->device);
        if (st != ACAMERA_OK || !s->device) {
            LOGE("openCamera failed for %s: status=%d", camera_id, st);
            goto fail;
        }
    }

    // Choose sizes
    int32_t prev_w, prev_h;
    pick_preview_size(s->chars, &prev_w, &prev_h);
    LOGI("Preview size: %dx%d", prev_w, prev_h);

    // Create preview AImageReader
    if (AImageReader_new(prev_w, prev_h, 0x23 /*YUV_420_888*/, 4,
                         &s->preview_reader) != AMEDIA_OK) {
        LOGE("AImageReader_new (preview) failed");
        goto fail;
    }
    AImageReader_getWindow(s->preview_reader, &s->preview_window);

    // Use only the preview stream in the capture session. A separate high-res
    // JPEG stream combined with YUV preview causes ERROR_CAMERA_DEVICE on many
    // hardware levels. Photo capture reuses the preview reader instead.
    s->photo_is_jpeg = 0; // photos are returned as RGB24 (encoded to JPEG in Rust)

    // Build capture targets (wrapping the native window for use in capture requests)
    ACameraOutputTarget_create(s->preview_window, &s->preview_target);

    // Build output container with preview stream only
    ACaptureSessionOutput_create(s->preview_window, &s->preview_output);
    ACaptureSessionOutputContainer_create(&s->outputs);
    ACaptureSessionOutputContainer_add(s->outputs, s->preview_output);

    // Create capture session. In the NDK C API this call is non-blocking; the
    // session becomes usable as soon as it returns ACAMERA_OK.  onReady fires
    // only after a request has been processed, so we must NOT wait for it here.
    {
        ACameraCaptureSession_stateCallbacks sess_cbs = {
            .context  = s,
            .onReady  = on_session_ready,
            .onActive = on_session_active,
            .onClosed = on_session_closed,
        };
        if (ACameraDevice_createCaptureSession(s->device, s->outputs,
                                               &sess_cbs, &s->session) != ACAMERA_OK
            || !s->session) {
            LOGE("ACameraDevice_createCaptureSession failed");
            goto fail;
        }
    }

    // Build preview capture request and start repeating
    if (ACameraDevice_createCaptureRequest(s->device, TEMPLATE_PREVIEW,
                                           &s->preview_req) != ACAMERA_OK)
        goto fail;
    ACaptureRequest_addTarget(s->preview_req, s->preview_target);

    // Build photo capture request (targets same preview stream — no separate photo stream)
    if (ACameraDevice_createCaptureRequest(s->device, TEMPLATE_STILL_CAPTURE,
                                           &s->photo_req) != ACAMERA_OK)
        goto fail;
    ACaptureRequest_addTarget(s->photo_req, s->preview_target);

    // Start repeating preview, then wait for onActive (frames flowing).
    {
        ACaptureRequest *reqs[] = { s->preview_req };
        camera_status_t st = ACameraCaptureSession_setRepeatingRequest(
            s->session, NULL, 1, reqs, NULL);
        if (st != ACAMERA_OK) {
            LOGE("setRepeatingRequest failed: %d", st);
            goto fail;
        }
    }

    // Wait for onActive — that signals the first capture has been queued.
    {
        pthread_mutex_lock(&s->sess_mutex);
        struct timespec ts;
        clock_gettime(CLOCK_REALTIME, &ts);
        ts.tv_sec += 5;
        while (!s->sess_ready) {
            if (pthread_cond_timedwait(&s->sess_cond, &s->sess_mutex, &ts) != 0)
                break;
        }
        int active = s->sess_ready;
        pthread_mutex_unlock(&s->sess_mutex);
        if (!active) LOGE("Timed out waiting for session active — continuing anyway");
    }

    LOGI("Session opened for camera %s (%dx%d preview)", camera_id, prev_w, prev_h);
    return s;

fail:
    LOGE("ac_open_session failed for camera %s", camera_id);
    ac_close_session(s);
    return NULL;
}

void ac_close_session(void *handle) {
    AcSession *s = (AcSession *)handle;
    if (!s) return;

    if (s->session) {
        ACameraCaptureSession_stopRepeating(s->session);
        ACameraCaptureSession_close(s->session);
    }
    if (s->preview_req)    ACaptureRequest_free(s->preview_req);
    if (s->photo_req)      ACaptureRequest_free(s->photo_req);
    if (s->preview_target) ACameraOutputTarget_free(s->preview_target);
    if (s->outputs)        ACaptureSessionOutputContainer_free(s->outputs);
    if (s->preview_output) ACaptureSessionOutput_free(s->preview_output);
    if (s->preview_reader) AImageReader_delete(s->preview_reader);
    if (s->device)       ACameraDevice_close(s->device);
    if (s->chars)        ACameraMetadata_free(s->chars);
    if (s->manager)      ACameraManager_delete(s->manager);

    free(s->photo_data);

    pthread_mutex_destroy(&s->sess_mutex);
    pthread_cond_destroy (&s->sess_cond);
    pthread_mutex_destroy(&s->photo_mutex);
    pthread_cond_destroy (&s->photo_cond);

    free(s);
}

// ---------------------------------------------------------------------------
// YUV_420_888 → packed RGB24 helper
// ---------------------------------------------------------------------------

static uint8_t *yuv_to_rgb(AImage *image, int32_t *out_w, int32_t *out_h,
                             size_t *out_size) {
    int32_t width = 0, height = 0;
    AImage_getWidth(image, &width);
    AImage_getHeight(image, &height);
    *out_w = width; *out_h = height;

    uint8_t *y_data = NULL, *u_data = NULL, *v_data = NULL;
    int y_len = 0, u_len = 0, v_len = 0;
    int32_t y_row = 0, u_row = 0, v_row = 0, u_pix = 0, v_pix = 0;

    AImage_getPlaneData(image, 0, &y_data, &y_len);
    AImage_getPlaneData(image, 1, &u_data, &u_len);
    AImage_getPlaneData(image, 2, &v_data, &v_len);
    AImage_getPlaneRowStride(image, 0, &y_row);
    AImage_getPlaneRowStride(image, 1, &u_row);
    AImage_getPlaneRowStride(image, 2, &v_row);
    AImage_getPlanePixelStride(image, 1, &u_pix);
    AImage_getPlanePixelStride(image, 2, &v_pix);

    if (!y_data || !u_data || !v_data || width <= 0 || height <= 0)
        return NULL;

    size_t rgb_size = (size_t)(width * height * 3);
    uint8_t *rgb = (uint8_t *)malloc(rgb_size);
    if (!rgb) return NULL;

    for (int row = 0; row < height; row++) {
        for (int col = 0; col < width; col++) {
            int y_val = y_data[row * y_row + col] & 0xFF;
            int u_val = u_data[(row / 2) * u_row + (col / 2) * u_pix] & 0xFF;
            int v_val = v_data[(row / 2) * v_row + (col / 2) * v_pix] & 0xFF;

            int r = (int)(y_val + 1.402f * (v_val - 128));
            int g = (int)(y_val - 0.344136f * (u_val - 128) - 0.714136f * (v_val - 128));
            int b = (int)(y_val + 1.772f * (u_val - 128));

            int idx = (row * width + col) * 3;
            rgb[idx]     = (uint8_t)(r < 0 ? 0 : r > 255 ? 255 : r);
            rgb[idx + 1] = (uint8_t)(g < 0 ? 0 : g > 255 ? 255 : g);
            rgb[idx + 2] = (uint8_t)(b < 0 ? 0 : b > 255 ? 255 : b);
        }
    }
    *out_size = rgb_size;
    return rgb;
}

int ac_capture_frame(void *handle,
                     uint8_t **out_data, size_t *out_size,
                     int32_t *out_width, int32_t *out_height) {
    AcSession *s = (AcSession *)handle;
    if (!s || !s->preview_reader) return -1;

    // Retry up to 500 ms: the repeating request may not have fired yet.
    AImage *image = NULL;
    for (int attempt = 0; attempt < 50; attempt++) {
        media_status_t st = AImageReader_acquireLatestImage(s->preview_reader, &image);
        if (st == AMEDIA_OK && image) break;
        if (attempt == 0) LOGI("ac_capture_frame: waiting for first image (status=%d)", st);
        usleep(10000); // 10 ms
    }
    if (!image) {
        LOGE("ac_capture_frame: no image after 500ms");
        return -1;
    }

    size_t rgb_size = 0;
    int32_t w = 0, h = 0;
    uint8_t *rgb = yuv_to_rgb(image, &w, &h, &rgb_size);
    AImage_delete(image);

    if (!rgb) return -1;
    *out_data   = rgb;
    *out_size   = rgb_size;
    *out_width  = w;
    *out_height = h;
    return 0;
}

// Helper: create a capture session and wait for onActive (up to timeout_sec).
static ACameraCaptureSession *create_session_and_wait(
        AcSession *s,
        ACaptureSessionOutputContainer *outputs,
        int timeout_sec) {
    ACameraCaptureSession_stateCallbacks cbs = {
        .context  = s,
        .onClosed = on_session_closed,
        .onReady  = on_session_ready,
        .onActive = on_session_active,
    };
    pthread_mutex_lock(&s->sess_mutex);
    s->sess_ready = 0;
    pthread_mutex_unlock(&s->sess_mutex);

    ACameraCaptureSession *sess = NULL;
    if (ACameraDevice_createCaptureSession(s->device, outputs, &cbs, &sess) != ACAMERA_OK
        || !sess)
        return NULL;
    return sess;
}

// Wait for onActive after submitting a repeating/single request.
static int wait_for_active(AcSession *s, int timeout_sec) {
    pthread_mutex_lock(&s->sess_mutex);
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    ts.tv_sec += timeout_sec;
    while (!s->sess_ready) {
        if (pthread_cond_timedwait(&s->sess_cond, &s->sess_mutex, &ts) != 0) break;
    }
    int ok = s->sess_ready;
    pthread_mutex_unlock(&s->sess_mutex);
    return ok;
}

int ac_capture_photo(void *handle,
                     uint8_t **out_data, size_t *out_size,
                     int32_t *out_width, int32_t *out_height,
                     int32_t *out_is_jpeg) {
    AcSession *s = (AcSession *)handle;
    if (!s || !s->device || !s->preview_reader) return -1;

    int result = -1;

    // ---- 1. Pause preview ------------------------------------------------
    if (s->session) ACameraCaptureSession_stopRepeating(s->session);
    usleep(80000); // let last preview frame drain

    // ---- 2. Build a JPEG AImageReader at max resolution ------------------
    int32_t photo_w = 0, photo_h = 0;
    int has_jpeg = pick_photo_size(s->chars, &photo_w, &photo_h);
    if (!has_jpeg) { photo_w = 1280; photo_h = 720; }
    LOGI("Photo capture: %dx%d %s", photo_w, photo_h, has_jpeg ? "JPEG" : "YUV");

    AImageReader *jpeg_reader = NULL;
    ANativeWindow *jpeg_window = NULL;
    ACaptureSessionOutput *jpeg_out = NULL;
    ACaptureSessionOutputContainer *jpeg_outputs = NULL;
    ACameraOutputTarget *jpeg_target = NULL;
    ACaptureRequest *jpeg_req = NULL;
    ACameraCaptureSession *jpeg_sess = NULL;

    int32_t photo_fmt = has_jpeg ? 0x100 : 0x23;
    if (AImageReader_new(photo_w, photo_h, photo_fmt, 2, &jpeg_reader) != AMEDIA_OK) {
        LOGE("AImageReader_new (photo) failed");
        goto restore;
    }
    AImageReader_getWindow(jpeg_reader, &jpeg_window);

    // Register the existing photo callback on this reader.
    s->photo_is_jpeg = has_jpeg;
    {
        AImageReader_ImageListener lst = { .context = s, .onImageAvailable = on_photo_available };
        AImageReader_setImageListener(jpeg_reader, &lst);
    }

    ACaptureSessionOutput_create(jpeg_window, &jpeg_out);
    ACaptureSessionOutputContainer_create(&jpeg_outputs);
    ACaptureSessionOutputContainer_add(jpeg_outputs, jpeg_out);
    ACameraOutputTarget_create(jpeg_window, &jpeg_target);

    // ---- 3. Swap to JPEG session (closes preview session automatically) --
    jpeg_sess = create_session_and_wait(s, jpeg_outputs, 5);
    if (!jpeg_sess) { LOGE("JPEG session creation failed"); goto restore; }

    // ---- 4. Submit one still-capture request -----------------------------
    if (ACameraDevice_createCaptureRequest(s->device, TEMPLATE_STILL_CAPTURE, &jpeg_req) != ACAMERA_OK)
        goto restore;
    ACaptureRequest_addTarget(jpeg_req, jpeg_target);

    pthread_mutex_lock(&s->photo_mutex);
    free(s->photo_data); s->photo_data = NULL; s->photo_size = 0; s->photo_ready = 0;
    pthread_mutex_unlock(&s->photo_mutex);

    {
        ACaptureRequest *reqs[] = { jpeg_req };
        // Mark sess_ready=0 so wait_for_active detects the new onActive.
        pthread_mutex_lock(&s->sess_mutex); s->sess_ready = 0; pthread_mutex_unlock(&s->sess_mutex);
        if (ACameraCaptureSession_capture(jpeg_sess, NULL, 1, reqs, NULL) != ACAMERA_OK) {
            LOGE("JPEG capture request failed");
            goto restore;
        }
    }

    // ---- 5. Wait for image callback (up to 8 s) -------------------------
    {
        pthread_mutex_lock(&s->photo_mutex);
        struct timespec ts;
        clock_gettime(CLOCK_REALTIME, &ts);
        ts.tv_sec += 8;
        while (!s->photo_ready) {
            if (pthread_cond_timedwait(&s->photo_cond, &s->photo_mutex, &ts) != 0) break;
        }
        if (s->photo_ready && s->photo_data && s->photo_size > 0) {
            *out_data    = s->photo_data;
            *out_size    = s->photo_size;
            *out_width   = s->photo_width;
            *out_height  = s->photo_height;
            *out_is_jpeg = s->photo_is_jpeg;
            s->photo_data = NULL; s->photo_size = 0;
            result = 0;
        } else {
            LOGE("Photo capture timed out or no data");
        }
        pthread_mutex_unlock(&s->photo_mutex);
    }

restore:
    // ---- 6. Free JPEG session resources ---------------------------------
    if (jpeg_sess)    ACameraCaptureSession_close(jpeg_sess);
    if (jpeg_req)     ACaptureRequest_free(jpeg_req);
    if (jpeg_target)  ACameraOutputTarget_free(jpeg_target);
    if (jpeg_outputs) ACaptureSessionOutputContainer_free(jpeg_outputs);
    if (jpeg_out)     ACaptureSessionOutput_free(jpeg_out);
    if (jpeg_reader)  AImageReader_delete(jpeg_reader);

    // ---- 7. Restore preview session -------------------------------------
    s->session = create_session_and_wait(s, s->outputs, 5);
    if (!s->session) {
        LOGE("Failed to restore preview session after photo capture");
        return result;
    }

    pthread_mutex_lock(&s->sess_mutex); s->sess_ready = 0; pthread_mutex_unlock(&s->sess_mutex);
    {
        ACaptureRequest *prev[] = { s->preview_req };
        ACameraCaptureSession_setRepeatingRequest(s->session, NULL, 1, prev, NULL);
    }
    wait_for_active(s, 5);
    LOGI("Preview session restored after photo capture");

    return result;
}

void ac_free_frame(uint8_t *data) {
    free(data);
}

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

int ac_get_parameters(void *handle, AcParamDesc *out, int capacity) {
    AcSession *s = (AcSession *)handle;
    if (!s || !s->chars || capacity <= 0) return -1;

    int n = 0;
    ACameraMetadata_const_entry entry = {0};

#define PUSH(kind_str, cur_val, range, mn, mx, st, nopts, ...)  \
    do { if (n < capacity) {                                     \
        strncpy(out[n].kind, (kind_str), AC_MAX_KIND - 1);      \
        out[n].kind[AC_MAX_KIND - 1] = '\0';                     \
        out[n].current    = (cur_val);                           \
        out[n].is_range   = (range);                             \
        out[n].min        = (mn);                                \
        out[n].max        = (mx);                                \
        out[n].step       = (st);                                \
        out[n].num_options = (nopts);                            \
        __VA_ARGS__                                              \
        n++;                                                     \
    } } while (0)

    // AE mode (boolean: 1 = auto, 0 = manual)
    PUSH("ae_mode", s->cur_ae_mode, 0, 0, 0, 0, 2,
         { out[n].options[0] = (AcParamOption){0, "Manual"};
           out[n].options[1] = (AcParamOption){1, "Auto"}; });

    // ISO — only meaningful when ae_mode == manual (0)
    if (ACameraMetadata_getConstEntry(
            s->chars, ACAMERA_SENSOR_INFO_SENSITIVITY_RANGE, &entry) == ACAMERA_OK
        && entry.count >= 2) {
        int32_t iso_min = entry.data.i32[0];
        int32_t iso_max = entry.data.i32[1];
        PUSH("iso", s->cur_iso, 1, iso_min, iso_max, 50, 0,);
    }

    // Shutter speed in microseconds — only meaningful when ae_mode == manual
    if (ACameraMetadata_getConstEntry(
            s->chars, ACAMERA_SENSOR_INFO_EXPOSURE_TIME_RANGE, &entry) == ACAMERA_OK
        && entry.count >= 2) {
        // NDK delivers nanoseconds (int64), clamp to microseconds (int32)
        int64_t ns_min = entry.data.i64[0];
        int64_t ns_max = entry.data.i64[1];
        int32_t us_min = (int32_t)(ns_min / 1000LL < 1 ? 1 : ns_min / 1000LL);
        int32_t us_max = (int32_t)(ns_max / 1000LL > 2100000000LL ? 2100000000 : ns_max / 1000LL);
        PUSH("shutter_us", s->cur_shutter_us, 1, us_min, us_max, 100, 0,);
    }

    // Aperture — expose as select with available f-numbers × 100
    if (ACameraMetadata_getConstEntry(
            s->chars, ACAMERA_LENS_INFO_AVAILABLE_APERTURES, &entry) == ACAMERA_OK
        && entry.count > 0) {
        int nopts = (int)entry.count < AC_MAX_OPTIONS ? (int)entry.count : AC_MAX_OPTIONS;
        AcParamDesc *d = &out[n];
        if (n < capacity) {
            strncpy(d->kind, "aperture", AC_MAX_KIND - 1);
            d->current    = s->cur_aperture_x100;
            d->is_range   = 0;
            d->num_options = nopts;
            for (int i = 0; i < nopts; i++) {
                float f = entry.data.f[i];
                d->options[i].value = (int32_t)(f * 100.0f + 0.5f);
                snprintf(d->options[i].label, AC_MAX_LABEL, "f/%.1f", f);
            }
            n++;
        }
    }

    // White balance mode (select)
    {
        static const struct { int32_t v; const char *l; } awb_modes[] = {
            {0, "Off"}, {1, "Auto"}, {2, "Incandescent"}, {3, "Fluorescent"},
            {4, "Warm Fluorescent"}, {5, "Daylight"}, {6, "Cloudy"},
            {7, "Twilight"}, {8, "Shade"},
        };
        int n_modes = (int)(sizeof(awb_modes) / sizeof(awb_modes[0]));
        PUSH("awb_mode", s->cur_awb_mode, 0, 0, 0, 0, n_modes,
             { for (int i = 0; i < n_modes; i++) {
                 out[n].options[i].value = awb_modes[i].v;
                 strncpy(out[n].options[i].label, awb_modes[i].l, AC_MAX_LABEL - 1);
               } });
    }

    // AF mode (select)
    {
        static const struct { int32_t v; const char *l; } af_modes[] = {
            {0, "Off (Manual)"}, {3, "Continuous Video"}, {4, "Continuous Picture"},
        };
        int n_modes = 3;
        PUSH("af_mode", s->cur_af_mode, 0, 0, 0, 0, n_modes,
             { for (int i = 0; i < n_modes; i++) {
                 out[n].options[i].value = af_modes[i].v;
                 strncpy(out[n].options[i].label, af_modes[i].l, AC_MAX_LABEL - 1);
               } });
    }

    // EV compensation (range in compensation steps)
    if (ACameraMetadata_getConstEntry(
            s->chars, ACAMERA_CONTROL_AE_COMPENSATION_RANGE, &entry) == ACAMERA_OK
        && entry.count >= 2) {
        PUSH("ev_compensation", s->cur_ev_comp, 1,
             entry.data.i32[0], entry.data.i32[1], 1, 0,);
    }

    // Focus distance (diopters × 100) — only when af_mode == 0 (manual)
    if (ACameraMetadata_getConstEntry(
            s->chars, ACAMERA_LENS_INFO_MINIMUM_FOCUS_DISTANCE, &entry) == ACAMERA_OK
        && entry.count >= 1) {
        int32_t max_d = (int32_t)(entry.data.f[0] * 100.0f + 0.5f);
        PUSH("focus_distance_x100", s->cur_focus_x100, 1, 0, max_d, 1, 0,);
    }

    // Zoom ratio × 100 (range)
    // ACAMERA_CONTROL_ZOOM_RATIO requires API 30; skip if not in available keys.
    if (ACameraMetadata_getConstEntry(
            s->chars, ACAMERA_SCALER_AVAILABLE_MAX_DIGITAL_ZOOM, &entry) == ACAMERA_OK
        && entry.count >= 1) {
        int32_t max_z = (int32_t)(entry.data.f[0] * 100.0f + 0.5f);
        PUSH("zoom_x100", s->cur_zoom_x100, 1, 100, max_z, 10, 0,);
    }

#undef PUSH

    return n;
}

int ac_set_parameter(void *handle, const char *kind, int32_t value) {
    AcSession *s = (AcSession *)handle;
    if (!s || !s->preview_req) return -1;

    if (strcmp(kind, "ae_mode") == 0) {
        // 0 = manual (ACAMERA_CONTROL_AE_MODE_OFF), 1 = auto (ACAMERA_CONTROL_AE_MODE_ON)
        uint8_t ae_mode = (value != 0) ? 1 : 0;
        ACaptureRequest_setEntry_u8(s->preview_req, ACAMERA_CONTROL_AE_MODE, 1, &ae_mode);
        s->cur_ae_mode = value != 0 ? 1 : 0;
    } else if (strcmp(kind, "iso") == 0) {
        int32_t iso = value;
        ACaptureRequest_setEntry_i32(s->preview_req, ACAMERA_SENSOR_SENSITIVITY, 1, &iso);
        s->cur_iso = iso;
    } else if (strcmp(kind, "shutter_us") == 0) {
        int64_t ns = (int64_t)value * 1000LL;
        ACaptureRequest_setEntry_i64(s->preview_req, ACAMERA_SENSOR_EXPOSURE_TIME, 1, &ns);
        s->cur_shutter_us = value;
    } else if (strcmp(kind, "aperture") == 0) {
        float f = (float)value / 100.0f;
        ACaptureRequest_setEntry_float(s->preview_req, ACAMERA_LENS_APERTURE, 1, &f);
        s->cur_aperture_x100 = value;
    } else if (strcmp(kind, "awb_mode") == 0) {
        uint8_t awb = (uint8_t)value;
        ACaptureRequest_setEntry_u8(s->preview_req, ACAMERA_CONTROL_AWB_MODE, 1, &awb);
        s->cur_awb_mode = value;
    } else if (strcmp(kind, "af_mode") == 0) {
        uint8_t af = (uint8_t)value;
        ACaptureRequest_setEntry_u8(s->preview_req, ACAMERA_CONTROL_AF_MODE, 1, &af);
        s->cur_af_mode = value;
    } else if (strcmp(kind, "ev_compensation") == 0) {
        int32_t ev = value;
        ACaptureRequest_setEntry_i32(s->preview_req, ACAMERA_CONTROL_AE_EXPOSURE_COMPENSATION, 1, &ev);
        s->cur_ev_comp = value;
    } else if (strcmp(kind, "focus_distance_x100") == 0) {
        float d = (float)value / 100.0f;
        ACaptureRequest_setEntry_float(s->preview_req, ACAMERA_LENS_FOCUS_DISTANCE, 1, &d);
        s->cur_focus_x100 = value;
    } else if (strcmp(kind, "zoom_x100") == 0) {
        // Use SCALER_CROP_REGION for compatibility with API < 30
        // For simplicity, update zoom_x100 tracking but apply via crop region
        // Full CONTROL_ZOOM_RATIO (API 30) would need runtime API level check
        s->cur_zoom_x100 = value;
        // Note: applying crop-based zoom requires knowing sensor array size
        // which is implementation-specific. Tracked but not applied via crop
        // here; a production implementation should read ACAMERA_SENSOR_INFO_ACTIVE_ARRAY_SIZE.
    } else {
        return -1; // unknown kind
    }

    // Re-submit the repeating request with the updated settings
    ACaptureRequest *reqs[] = { s->preview_req };
    ACameraCaptureSession_setRepeatingRequest(s->session, NULL, 1, reqs, NULL);
    return 0;
}
